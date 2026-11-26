use crate::config::SshHost;
use eframe::egui::text::LayoutJob;
use std::{
    borrow::Cow,
    collections::HashMap,
    net::{Ipv4Addr, Ipv6Addr},
    ops::Range,
};

fn sensitive_name(name: &str) -> bool {
    let name = name
        .trim_start_matches('-')
        .to_ascii_lowercase()
        .replace('-', "_");
    [
        "key",
        "token",
        "secret",
        "password",
        "passwd",
        "authorization",
        "credential",
    ]
    .iter()
    .any(|suffix| name.ends_with(suffix))
}

fn skip_spaces(bytes: &[u8], mut index: usize) -> usize {
    while matches!(bytes.get(index), Some(b' ' | b'\t')) {
        index += 1;
    }
    index
}

fn value_range(text: &str, start: usize) -> Range<usize> {
    let bytes = text.as_bytes();
    let quote = bytes
        .get(start)
        .copied()
        .filter(|b| matches!(b, b'"' | b'\''));
    let start = start + usize::from(quote.is_some());
    let mut end = start;
    while let Some(&b) = bytes.get(end) {
        if b == b'\n'
            || Some(b) == quote
            || (quote.is_none() && (b.is_ascii_whitespace() || b",;&|\x22\x27)}]".contains(&b)))
        {
            break;
        }
        if b == b'\\' && end + 1 < bytes.len() {
            end += 1;
        }
        end += 1;
    }
    start..end
}

fn secret_ranges(text: &str) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        while bytes
            .get(i)
            .is_some_and(|b| b.is_ascii_alphanumeric() || b"_-".contains(b))
        {
            i += 1;
        }
        if start == i {
            i += 1;
            continue;
        }
        let name = &text[start..i];
        if name == "-----BEGIN" {
            let header = text[start..].lines().next().unwrap_or_default();
            if header.ends_with("PRIVATE KEY-----") {
                let footer = header.replacen("BEGIN", "END", 1);
                let body = start + header.len();
                i = text[body..]
                    .find(&footer)
                    .map_or(text.len(), |n| body + n + footer.len());
                ranges.push(start..i);
                continue;
            }
        }
        let mut after = i;
        if matches!(bytes.get(after), Some(b'"' | b'\'')) {
            after += 1;
        }
        let separator = skip_spaces(bytes, after);
        let range = if sensitive_name(name) && matches!(bytes.get(separator), Some(b'=' | b':')) {
            let value = skip_spaces(bytes, separator + 1);
            // Authorization headers include the scheme before the credential.
            let scheme_end = text[value..].find([' ', '\t', '\n']).map(|n| value + n);
            let value = scheme_end
                .filter(|&end| {
                    ["bearer", "basic"]
                        .iter()
                        .any(|scheme| text[value..end].eq_ignore_ascii_case(scheme))
                })
                .map_or(value, |end| skip_spaces(bytes, end));
            value_range(text, value)
        } else if separator > i
            && (name.eq_ignore_ascii_case("bearer")
                || (name.starts_with("--") && sensitive_name(name)))
        {
            value_range(text, separator)
        } else if [
            "sk-",
            "ghp_",
            "gho_",
            "github_pat_",
            "xoxb-",
            "xoxp-",
            "sk_live_",
            "sk_test_",
            "AIza",
            "AKIA",
            "ASIA",
        ]
        .iter()
        .any(|prefix| name.starts_with(prefix))
        {
            value_range(text, start)
        } else {
            continue;
        };
        if !range.is_empty() {
            i = range.end;
            ranges.push(range);
        }
    }
    ranges
}

// Match shell path arguments, including quoted names, escaped spaces and
// host:/path prompts. Leave ordinary web URLs readable.
fn path_ranges(text: &str) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let quoted = matches!(bytes[i], b'\'' | b'"');
        if !quoted && (bytes[i].is_ascii_whitespace() || b"=:,;&|()[]{}<>".contains(&bytes[i])) {
            i += 1;
            continue;
        }
        let start = i + usize::from(quoted);
        let mut end = start;
        while end < bytes.len() {
            let b = bytes[end];
            if b == b'\n'
                || (quoted && b == bytes[i])
                || (!quoted && (b.is_ascii_whitespace() || b"=,;&|()[]{}<>\"'".contains(&b)))
            {
                break;
            }
            if b == b'\\'
                && bytes
                    .get(end + 1)
                    .is_some_and(|c| c.is_ascii_whitespace() || matches!(c, b'"' | b'\''))
            {
                end += 1;
            }
            end += 1;
        }
        let token = &text[start..end];
        if !token.contains("://") || token.starts_with("file://") {
            let mut path_start = start;
            // Keep the SSH prompt's user and host visible.
            if let Some((host, path)) = token.split_once(':')
                && !host.contains(['/', '\\'])
                && (path.starts_with('/') || path.starts_with('~'))
            {
                path_start += host.len() + 1;
            }
            let path = &text[path_start..end];
            if path.contains('/') || path.contains('\\') || path == "~" {
                let path_end = if quoted {
                    end
                } else {
                    path.trim_end_matches(['$', '%', '#']).len() + path_start
                };
                if path_end > path_start {
                    ranges.push(path_start..path_end);
                }
            }
        }
        i = end + usize::from(quoted && bytes.get(end) == Some(&bytes[i]));
    }
    ranges
}

#[derive(Default)]
struct Labels(HashMap<String, String>);

impl Labels {
    fn remember(&mut self, value: &str, prefix: &str) {
        if value.is_empty() {
            return;
        }
        let next = self.0.len() + 1;
        self.0
            .entry(value.to_ascii_lowercase())
            .or_insert_with(|| format!("{prefix}-{next}"));
    }
}

pub struct Identities {
    users: Labels,
    hosts: Labels,
    ips: Labels,
}

impl Default for Identities {
    fn default() -> Self {
        let mut ids = Self {
            users: Labels::default(),
            hosts: Labels::default(),
            ips: Labels::default(),
        };
        for var in ["USER", "LOGNAME"] {
            if let Ok(name) = std::env::var(var) {
                ids.users.remember(&name, "user");
            }
        }
        if let Ok(output) = std::process::Command::new("/bin/hostname").output() {
            let name = String::from_utf8_lossy(&output.stdout);
            ids.hosts.remember(name.trim(), "host");
            if let Some((short, _)) = name.trim().split_once('.') {
                ids.hosts.remember(short, "host");
            }
        }
        ids
    }
}

impl Identities {
    fn masks(&mut self, text: &str, configured: &[SshHost]) -> Vec<Mask> {
        for host in configured {
            self.users.remember(&host.user, "user");
            for name in [&host.address, &host.name, &host.alias] {
                self.hosts.remember(name, "host");
            }
        }
        let mut words = Vec::new();
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let start = i;
            // IPv6 literals may contain compressed groups and embedded IPv4.
            if bytes[i].is_ascii_hexdigit() || bytes[i] == b':' {
                let mut end = i;
                while bytes
                    .get(end)
                    .is_some_and(|b| b.is_ascii_hexdigit() || matches!(b, b':' | b'.'))
                {
                    end += 1;
                }
                let literal = text[start..end].trim_end_matches('.');
                if let Ok(ip) = literal.parse::<Ipv6Addr>() {
                    end = start + literal.len();
                    let key = ip.to_string();
                    self.ips.remember(&key, "ip");
                    words.push((start..end, key, true));
                    i = end;
                    continue;
                }
            }
            while bytes
                .get(i)
                .is_some_and(|b| b.is_ascii_alphanumeric() || b"._-".contains(b))
            {
                i += 1;
            }
            if i == start {
                i += 1;
                continue;
            }
            let word = text[start..i].trim_end_matches('.');
            if word.is_empty() {
                continue;
            }
            let end = start + word.len();
            let key = word.to_ascii_lowercase();
            let is_ip = word.parse::<Ipv4Addr>().is_ok();
            if is_ip {
                self.ips.remember(&key, "ip");
            } else if bytes.get(end) == Some(&b'@')
                || text[..start].ends_with("/Users/")
                || [
                    "/home/",
                    "USER=",
                    "USERNAME=",
                    "LOGNAME=",
                    "whoami\n",
                    "user=",
                    "username=",
                ]
                .iter()
                .any(|prefix| text[..start].ends_with(prefix))
            {
                self.users.remember(&key, "user");
            } else if bytes.get(start.wrapping_sub(1)) == Some(&b'@')
                || ["ssh ", "HOSTNAME=", "HOST=", "hostname=", "hostname\n"]
                    .iter()
                    .any(|prefix| text[..start].ends_with(prefix))
                || (word.contains('.')
                    && word.rsplit('.').next().is_some_and(|s| {
                        s.len() >= 2 && s.bytes().all(|b| b.is_ascii_alphabetic())
                    }))
            {
                self.hosts.remember(&key, "host");
            }
            words.push((start..end, key, is_ip));
        }
        // Discover names first, so an earlier line is masked even if the prompt is below it.
        words
            .into_iter()
            .filter_map(|(range, key, ip)| {
                let label = if ip {
                    self.ips.0.get(&key)
                } else if range.start > 0 && bytes[range.start - 1] == b'@' {
                    self.hosts.0.get(&key)
                } else {
                    self.users.0.get(&key).or_else(|| self.hosts.0.get(&key))
                }?;
                Some(Mask {
                    range,
                    marker: Cow::Owned(label.clone()),
                })
            })
            .collect()
    }
}

struct Mask {
    range: Range<usize>,
    marker: Cow<'static, str>,
}

// Scan the assembled text so terminal colors and soft wraps cannot split a match.
// Rebuild only the projected text and map its caret to the shortened display.
pub fn apply(
    job: LayoutJob,
    cursor: Option<usize>,
    mask_keys: bool,
    mask_paths: bool,
    identities: Option<(&mut Identities, &[SshHost])>,
) -> (LayoutJob, Option<usize>) {
    let mut ranges = if mask_keys {
        secret_ranges(&job.text)
            .into_iter()
            .map(|range| Mask {
                range,
                marker: "***".into(),
            })
            .collect()
    } else {
        Vec::new()
    };
    if mask_paths {
        ranges.extend(path_ranges(&job.text).into_iter().map(|range| Mask {
            range,
            marker: ".../...".into(),
        }));
    }
    let has_identities = identities.is_some();
    if let Some((ids, configured)) = identities {
        ranges.extend(ids.masks(&job.text, configured));
    }
    if mask_paths || has_identities {
        ranges.sort_unstable_by_key(|mask| mask.range.start);
        ranges.dedup_by(|later, earlier| {
            if later.range.start < earlier.range.end {
                earlier.range.end = earlier.range.end.max(later.range.end);
                if later.marker == "***" {
                    earlier.marker = "***".into();
                } else if earlier.marker != "***" && later.marker == ".../..." {
                    earlier.marker = ".../...".into();
                }
                true
            } else {
                false
            }
        });
    }
    if ranges.is_empty() {
        return (job, cursor);
    }
    let mut result = LayoutJob {
        wrap: job.wrap,
        keep_trailing_whitespace: job.keep_trailing_whitespace,
        ..Default::default()
    };
    let mut ranges = ranges.into_iter().peekable();
    let mut source_chars = 0;
    let mut display_chars = 0;
    let mut mapped = None;
    for section in &job.sections {
        let mut pos = section.byte_range.start.0;
        let end = section.byte_range.end.0;
        while pos < end {
            let mask = ranges.peek();
            let range = mask.map(|m| &m.range);
            let hidden = range.is_some_and(|r| pos >= r.start);
            let stop = range.map_or(end, |r| end.min(if hidden { r.end } else { r.start }));
            let original = &job.text[pos..stop];
            let replacement = if hidden {
                mask.filter(|m| m.range.start == pos)
                    .map_or("", |m| m.marker.as_ref())
            } else {
                original
            };
            result.append(replacement, 0.0, section.format.clone());
            let count = original.chars().count();
            if let Some(c) = cursor
                && c >= source_chars
                && c < source_chars + count
            {
                mapped = Some(display_chars + if hidden { 0 } else { c - source_chars });
            }
            source_chars += count;
            display_chars += replacement.chars().count();
            pos = stop;
            if hidden && range.is_some_and(|r| r.end == stop) {
                ranges.next();
            }
        }
    }
    (result, cursor.map(|_| mapped.unwrap_or(display_chars)))
}
