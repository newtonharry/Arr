//! Torrent identity: magnet `xt` parsing and the info-hash of a `.torrent` file (SHA-1 of the
//! bencoded `info` dict for v1/hybrid; truncated SHA-256 for v2-only, as qBittorrent's
//! torrent ID). Identity is computed and persisted before anything is submitted.

use anyhow::{Context, Result, bail};
use sha1::{Digest, Sha1};
use std::ops::Range;

/// A bencoded value; dictionaries keep each value's byte span.
#[derive(Debug)]
pub enum Bencode<'a> {
    Int(i64),
    Bytes(&'a [u8]),
    List(Vec<Bencode<'a>>),
    Dict(Vec<(&'a [u8], Bencode<'a>, Range<usize>)>),
}

impl<'a> Bencode<'a> {
    pub fn get(&self, key: &str) -> Option<(&Bencode<'a>, &Range<usize>)> {
        match self {
            Bencode::Dict(entries) => entries
                .iter()
                .find(|(k, _, _)| *k == key.as_bytes())
                .map(|(_, v, r)| (v, r)),
            _ => None,
        }
    }

    pub fn bytes(&self) -> Option<&'a [u8]> {
        match self {
            Bencode::Bytes(b) => Some(b),
            _ => None,
        }
    }

    pub fn int(&self) -> Option<i64> {
        match self {
            Bencode::Int(i) => Some(*i),
            _ => None,
        }
    }
}

const MAX_DEPTH: usize = 32;
/// Decoded values per document at most (a 10 MiB degenerate torrent cannot exhaust memory).
const MAX_NODES: usize = 500_000;

struct Parser<'a> {
    input: &'a [u8],
    nodes: usize,
}

impl<'a> Parser<'a> {
    fn value(&mut self, pos: usize, depth: usize) -> Result<(Bencode<'a>, usize)> {
        if depth > MAX_DEPTH {
            bail!("bencode nesting too deep");
        }
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            bail!("bencode document has too many values");
        }
        let input = self.input;
        match input.get(pos).copied() {
            Some(b'i') => {
                let end = input[pos..]
                    .iter()
                    .position(|&b| b == b'e')
                    .context("unterminated integer")?
                    + pos;
                let s = std::str::from_utf8(&input[pos + 1..end])?;
                if s.is_empty() || (s.starts_with('0') && s.len() > 1) || s.starts_with("-0") {
                    bail!("non-canonical integer");
                }
                Ok((Bencode::Int(s.parse()?), end + 1))
            }
            Some(b'l') => {
                let mut items = vec![];
                let mut p = pos + 1;
                while input.get(p) != Some(&b'e') {
                    let (v, next) = self.value(p, depth + 1)?;
                    items.push(v);
                    p = next;
                }
                Ok((Bencode::List(items), p + 1))
            }
            Some(b'd') => {
                let mut entries: Vec<(&'a [u8], Bencode<'a>, Range<usize>)> = vec![];
                let mut p = pos + 1;
                while input.get(p) != Some(&b'e') {
                    let (k, next) = self.value(p, depth + 1)?;
                    let Bencode::Bytes(k) = k else {
                        bail!("dictionary key is not a string")
                    };
                    // A duplicate key (e.g. two `info` dictionaries) could make this parser
                    // and the client disagree about the torrent's identity.
                    if entries.iter().any(|(e, _, _)| *e == k) {
                        bail!("duplicate dictionary key");
                    }
                    let (v, end) = self.value(next, depth + 1)?;
                    entries.push((k, v, next..end));
                    p = end;
                }
                Ok((Bencode::Dict(entries), p + 1))
            }
            Some(b'0'..=b'9') => {
                let colon = input[pos..]
                    .iter()
                    .position(|&b| b == b':')
                    .context("bad string length")?
                    + pos;
                let len: usize = std::str::from_utf8(&input[pos..colon])?.parse()?;
                let start = colon + 1;
                let end = start
                    .checked_add(len)
                    .filter(|e| *e <= input.len())
                    .context("string runs past the end")?;
                Ok((Bencode::Bytes(&input[start..end]), end))
            }
            _ => bail!("invalid bencode at byte {pos}"),
        }
    }
}

/// Parses a complete bencoded document.
pub fn parse(input: &[u8]) -> Result<Bencode<'_>> {
    let mut parser = Parser { input, nodes: 0 };
    let (v, end) = parser.value(0, 0)?;
    if end != input.len() {
        bail!("trailing data after bencoded document");
    }
    Ok(v)
}

/// Torrent name and file paths (`/`-joined) from a `.torrent`, for identity checks before
/// submission. A single-file torrent lists its name as its only file.
pub fn names(torrent: &[u8]) -> Result<(String, Vec<String>)> {
    let doc = parse(torrent)?;
    let (info, _) = doc.get("info").context("torrent has no info dictionary")?;
    let text = |b: &[u8]| {
        String::from_utf8_lossy(b)
            .chars()
            .take(300)
            .collect::<String>()
    };
    let name = info
        .get("name")
        .and_then(|(n, _)| n.bytes())
        .map(text)
        .context("torrent info has no name")?;
    let mut files = vec![];
    if let Some((Bencode::List(list), _)) = info.get("files") {
        for f in list.iter().take(10_000) {
            if let Some((Bencode::List(parts), _)) = f.get("path") {
                let parts: Vec<String> = parts.iter().filter_map(|p| p.bytes().map(text)).collect();
                files.push(parts.join("/"));
            }
        }
    } else {
        files.push(name.clone());
    }
    Ok((name, files))
}

/// Info-hash (lowercase hex, 40 chars) of a `.torrent` file, after validating its shape.
pub fn info_hash(torrent: &[u8]) -> Result<String> {
    let doc = parse(torrent).context("not a valid .torrent (bencode)")?;
    let (info, span) = doc.get("info").context("torrent has no info dictionary")?;
    if !matches!(info, Bencode::Dict(_)) {
        bail!("torrent info is not a dictionary");
    }
    if info
        .get("name")
        .and_then(|(n, _)| n.bytes())
        .is_none_or(|n| n.is_empty())
    {
        bail!("torrent info has no name");
    }
    if info
        .get("piece length")
        .and_then(|(p, _)| p.int())
        .is_none_or(|p| p <= 0)
    {
        bail!("torrent info has no piece length");
    }
    let raw = &torrent[span.clone()];
    if info.get("pieces").is_some() {
        return Ok(hex::encode(Sha1::digest(raw)));
    }
    if info.get("meta version").and_then(|(v, _)| v.int()) == Some(2) {
        return Ok(hex::encode(&sha2::Sha256::digest(raw)[..20]));
    }
    bail!("torrent info has neither v1 pieces nor meta version 2")
}

fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut bits: u64 = 0;
    let mut n = 0;
    let mut out = vec![];
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a',
            b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        };
        bits = (bits << 5) | u64::from(v);
        n += 5;
        if n >= 8 {
            n -= 8;
            out.push((bits >> n) as u8);
        }
    }
    Some(out)
}

/// Info-hash of a magnet URI (`xt=urn:btih:` hex or base32; `urn:btmh:1220…` v2 only).
pub fn magnet_hash(uri: &str) -> Result<String> {
    if !uri.starts_with("magnet:?") || uri.len() > 16 * 1024 {
        bail!("not a magnet URI");
    }
    let url = reqwest::Url::parse(uri).context("unparseable magnet URI")?;
    let mut v2 = None;
    for (k, v) in url.query_pairs() {
        if k != "xt" {
            continue;
        }
        if let Some(h) = v.strip_prefix("urn:btih:") {
            if h.len() == 40 && h.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Ok(h.to_ascii_lowercase());
            }
            if h.len() == 32
                && let Some(bytes) = base32_decode(h)
                && bytes.len() == 20
            {
                return Ok(hex::encode(bytes));
            }
            bail!("magnet btih is neither 40 hex nor 32 base32 characters");
        }
        if let Some(h) = v.strip_prefix("urn:btmh:1220")
            && h.len() == 64
            && h.bytes().all(|b| b.is_ascii_hexdigit())
        {
            v2 = Some(h[..40].to_ascii_lowercase());
        }
    }
    v2.context("magnet URI has no BitTorrent info-hash")
}

/// A 40-hex lowercase info-hash, as qBittorrent reports it.
pub fn is_info_hash(h: &str) -> bool {
    h.len() == 40 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magnets() {
        let hex = "c12fe1c06bba254a9dc9f519b335aa7c1367a88a";
        assert_eq!(
            magnet_hash(&format!("magnet:?xt=urn:btih:{}&dn=x", hex.to_uppercase())).unwrap(),
            hex
        );
        // RFC 4648 base32 of the same 20 bytes.
        let b32 = "YEX6DQDLXISUVHOJ6UM3GNNKPQJWPKEK";
        assert_eq!(
            magnet_hash(&format!("magnet:?xt=urn:btih:{b32}")).unwrap(),
            hex
        );
        assert!(magnet_hash("http://example/x.torrent").is_err());
        assert!(magnet_hash("magnet:?dn=nothing").is_err());
        assert!(magnet_hash("magnet:?xt=urn:btih:zz").is_err());
        let v2 = "a".repeat(64);
        assert_eq!(
            magnet_hash(&format!("magnet:?xt=urn:btmh:1220{v2}")).unwrap(),
            "a".repeat(40)
        );
    }

    #[test]
    fn torrents() {
        let info =
            b"d6:lengthi3e4:name5:a.mkv12:piece lengthi16384e6:pieces20:aaaaaaaaaaaaaaaaaaaae";
        let mut t = b"d8:announce0:4:info".to_vec();
        t.extend_from_slice(info);
        t.push(b'e');
        assert_eq!(info_hash(&t).unwrap(), hex::encode(Sha1::digest(info)));
        assert!(
            info_hash(b"d4:infod4:name1:aee").is_err(),
            "no piece length"
        );
        assert!(info_hash(b"not bencode").is_err());
        assert!(info_hash(&[b'l'; 100]).is_err(), "deep nesting is bounded");
        let mut trailing = t.clone();
        trailing.push(b'x');
        assert!(info_hash(&trailing).is_err());
        // Duplicate keys are refused (two `info` dictionaries would make identity ambiguous).
        let mut dup = b"d4:info".to_vec();
        dup.extend_from_slice(info);
        dup.extend_from_slice(b"4:info");
        dup.extend_from_slice(info);
        dup.push(b'e');
        assert!(format!("{:#}", info_hash(&dup).unwrap_err()).contains("duplicate"));
        assert!(parse(b"d1:ai1e1:ai2ee").is_err());
        // A degenerate document with too many values is refused.
        let mut many = b"l".to_vec();
        many.extend(std::iter::repeat_n(b"i0e".as_slice(), 600_000).flatten());
        many.push(b'e');
        assert!(
            parse(&many)
                .unwrap_err()
                .to_string()
                .contains("too many values")
        );
        // The raw info span (and so the hash) is unchanged for well-formed torrents.
        assert_eq!(info_hash(&t).unwrap(), hex::encode(Sha1::digest(info)));
        assert_eq!(
            names(&t).unwrap(),
            ("a.mkv".to_string(), vec!["a.mkv".to_string()])
        );
    }
}
