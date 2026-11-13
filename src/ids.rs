//! Typed media identifiers.
//!
//! Canonical string forms (strict: digits only, no leading zeros, no zero values):
//! `movie:tmdb:603`, `series:tvdb:372264`, `season:tvdb:372264:1`,
//! `episode:tvdb:372264:1:3`. Series may also use the `tvmaze` namespace when a show has no
//! known TVDB id. Season 0 (specials) is rejected: special numbering is not supported.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

pub const MAX_SEASON: u16 = 999;
pub const MAX_EPISODE: u16 = 9999;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SeriesKey {
    Tvdb(u32),
    Tvmaze(u32),
}

/// One sport event, or one part of it: `event:<league>:<espn id>[:<part>]`. The league is the
/// catalogue's own slug, so the id stays `Copy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EventRef {
    pub league: &'static str,
    pub espn: u64,
    pub part: crate::sport::Part,
}

impl EventRef {
    /// The event's own id, without a part: how `sport_events` and `sport_files` key it.
    pub fn event_id(&self) -> String {
        format!("event:{}:{}", self.league, self.espn)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MediaId {
    Movie {
        tmdb: u32,
    },
    Series(SeriesKey),
    Season(SeriesKey, u16),
    Episode(SeriesKey, u16, u16),
    /// A sport event (or a part of one). It is not a title: it has no catalogue entry, no
    /// profile and no `media` row, and it is kept out of every movie and series list.
    Event(EventRef),
}

impl MediaId {
    pub fn kind(&self) -> &'static str {
        match self {
            MediaId::Movie { .. } => "movie",
            MediaId::Series(_) => "series",
            MediaId::Season(..) => "season",
            MediaId::Episode(..) => "episode",
            MediaId::Event(_) => "event",
        }
    }

    pub fn series_key(&self) -> Option<SeriesKey> {
        match *self {
            MediaId::Movie { .. } | MediaId::Event(_) => None,
            MediaId::Series(k) | MediaId::Season(k, _) | MediaId::Episode(k, _, _) => Some(k),
        }
    }
}

impl fmt::Display for SeriesKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SeriesKey::Tvdb(id) => write!(f, "tvdb:{id}"),
            SeriesKey::Tvmaze(id) => write!(f, "tvmaze:{id}"),
        }
    }
}

impl fmt::Display for MediaId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MediaId::Movie { tmdb } => write!(f, "movie:tmdb:{tmdb}"),
            MediaId::Series(k) => write!(f, "series:{k}"),
            MediaId::Season(k, s) => write!(f, "season:{k}:{s}"),
            MediaId::Episode(k, s, e) => write!(f, "episode:{k}:{s}:{e}"),
            MediaId::Event(e) => {
                write!(f, "event:{}:{}", e.league, e.espn)?;
                match e.part.slug() {
                    Some(p) => write!(f, ":{p}"),
                    None => Ok(()),
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaIdError(pub String);

impl fmt::Display for MediaIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for MediaIdError {}

fn number(part: &str, what: &str, max: u32) -> Result<u32, MediaIdError> {
    let valid = !part.is_empty()
        && part.len() <= 10
        && part.bytes().all(|b| b.is_ascii_digit())
        && !part.starts_with('0');
    let value = if valid {
        part.parse::<u64>().ok()
    } else {
        None
    };
    match value {
        Some(v) if v >= 1 && v <= u64::from(max) => Ok(v as u32),
        _ => Err(MediaIdError(format!(
            "{what} must be a positive integer without leading zeros (max {max})"
        ))),
    }
}

impl FromStr for MediaId {
    type Err = MediaIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() > 64 {
            return Err(MediaIdError("media id is too long".into()));
        }
        let parts: Vec<&str> = s.split(':').collect();
        let series_key = |ns: &str, id: &str| -> Result<SeriesKey, MediaIdError> {
            let id = number(id, "series id", u32::MAX)?;
            match ns {
                "tvdb" => Ok(SeriesKey::Tvdb(id)),
                "tvmaze" => Ok(SeriesKey::Tvmaze(id)),
                _ => Err(MediaIdError(
                    "series namespace must be tvdb or tvmaze".into(),
                )),
            }
        };
        let event = |slug: &str, id: &str, part| -> Result<MediaId, MediaIdError> {
            let league = crate::sport::catalog::find(slug)
                .ok_or_else(|| MediaIdError("event league is not one Arr knows".into()))?;
            let valid = !id.is_empty()
                && id.len() <= 18
                && id.bytes().all(|b| b.is_ascii_digit())
                && !id.starts_with('0');
            let espn = id.parse::<u64>().ok().filter(|_| valid).ok_or_else(|| {
                MediaIdError("event id must be digits without leading zeros".into())
            })?;
            Ok(MediaId::Event(EventRef {
                league: league.slug,
                espn,
                part,
            }))
        };
        let season = |p: &str| number(p, "season", u32::from(MAX_SEASON)).map(|v| v as u16);
        let episode = |p: &str| number(p, "episode", u32::from(MAX_EPISODE)).map(|v| v as u16);
        match parts.as_slice() {
            ["movie", "tmdb", id] => Ok(MediaId::Movie {
                tmdb: number(id, "tmdb id", u32::MAX)?,
            }),
            ["movie", ..] => Err(MediaIdError("movie ids must be movie:tmdb:<id>".into())),
            ["series", ns, id] => Ok(MediaId::Series(series_key(ns, id)?)),
            ["season", ns, id, s] => Ok(MediaId::Season(series_key(ns, id)?, season(s)?)),
            ["episode", ns, id, s, e] => Ok(MediaId::Episode(
                series_key(ns, id)?,
                season(s)?,
                episode(e)?,
            )),
            ["event", slug, id] => event(slug, id, crate::sport::Part::Whole),
            ["event", slug, id, part] => match crate::sport::Part::from_slug(part) {
                Some(p) => event(slug, id, p),
                None => Err(MediaIdError("event part is not one a sport has".into())),
            },
            _ => Err(MediaIdError(
                "expected movie:tmdb:<id>, series:<tvdb|tvmaze>:<id>, season:<ns>:<id>:<season>, episode:<ns>:<id>:<season>:<episode> or event:<league>:<id>[:<part>]".into(),
            )),
        }
    }
}

impl Serialize for MediaId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for MediaId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_canonical_forms() {
        for s in [
            "movie:tmdb:603",
            "series:tvdb:372264",
            "series:tvmaze:1",
            "season:tvdb:372264:1",
            "episode:tvdb:372264:1:3",
            "episode:tvmaze:77:12:104",
        ] {
            let id: MediaId = s.parse().unwrap();
            assert_eq!(id.to_string(), s);
        }
    }

    #[test]
    fn rejects_non_canonical_and_hostile_input() {
        for s in [
            "",
            "movie:603",
            "movie:imdb:tt0133093",
            "movie:tmdb:0",
            "movie:tmdb:0603",
            "movie:tmdb:-1",
            "movie:tmdb:+5",
            "movie:tmdb:99999999999",
            "series:tmdb:1",
            "season:tvdb:1:0",
            "season:tvdb:1:01",
            "season:tvdb:1:1000",
            "episode:tvdb:1:1",
            "episode:tvdb:1:1:0",
            "season:tvdb:1:1:extra",
            "series:tvdb:1 ",
            "series:tvdb:１",
            "movie:tmdb:603/../../etc",
        ] {
            assert!(s.parse::<MediaId>().is_err(), "{s} should be rejected");
        }
    }

    #[test]
    fn serde_uses_string_form() {
        let id = MediaId::Season(SeriesKey::Tvdb(5), 2);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"season:tvdb:5:2\"");
        assert_eq!(serde_json::from_str::<MediaId>(&json).unwrap(), id);
        assert!(serde_json::from_str::<MediaId>("\"season:tvdb:5:0\"").is_err());
    }
}
