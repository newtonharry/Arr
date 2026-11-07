//! Payload identity: whether a torrent, folder or file name could belong to a managed item.
//!
//! Names are judged with the same title keys as release decisions (`catalog::*_title_keys`:
//! title and aliases, with and without a trailing year, plus franchise-prefixed alias forms
//! the metadata implies). A name **contradicts** the item when it is clearly another
//! release: its title differs and it carries release structure (a year for movies, season or
//! episode numbering for TV), or, for a movie, its title matches but its year does not.
//! Obfuscated or partial names (`abc123.mkv`, `S01E02.mkv`, `movie.mkv`) are **neutral** and
//! rely on the validated grab context. A contradiction is never overridden by the approved
//! search title.

use crate::catalog::{Movie, Series, movie_title_keys, series_title_keys};
use crate::release::{self, Numbering};
use crate::title;

#[derive(Debug, Clone)]
pub struct Identity {
    pub label: String,
    keys: Vec<String>,
    years: Vec<u16>,
    movie: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    Consistent,
    Neutral,
    Contradicts(String),
}

const VIDEO_EXTENSIONS: &[&str] = &[
    "mkv", "mk3d", "webm", "mp4", "m4v", "mov", "avi", "ts", "m2ts", "mpg", "mpeg", "wmv", "asf",
    "iso", "img", "vob",
];

impl Identity {
    pub fn movie(m: &Movie) -> Self {
        Identity {
            label: match m.year {
                Some(y) => format!("{} ({y})", m.title),
                None => m.title.clone(),
            },
            keys: movie_title_keys(m),
            years: m.year.into_iter().chain(m.secondary_year).collect(),
            movie: true,
        }
    }

    pub fn series(s: &Series) -> Self {
        Identity {
            label: s.title.clone(),
            keys: series_title_keys(s, None),
            years: vec![],
            movie: false,
        }
    }

    /// Judges one name (a torrent name, a folder, or a file name with its extension).
    pub fn check(&self, name: &str) -> Check {
        let base = name.rsplit('/').next().unwrap_or(name).trim();
        let stem = match base.rsplit_once('.') {
            Some((stem, ext)) if VIDEO_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()) => {
                stem
            }
            _ => base,
        };
        let p = release::parse(stem);
        if p.title_tokens.is_empty() {
            return Check::Neutral;
        }
        let key_a = title::key_from_tokens(&p.title_tokens);
        let key_b = p.year.map(|y| {
            let mut t = p.title_tokens.clone();
            t.push(y.to_string());
            title::key_from_tokens(&t)
        });
        let tv_numbering = matches!(
            p.numbering,
            Numbering::Episodes { .. }
                | Numbering::Season { .. }
                | Numbering::Seasons { .. }
                | Numbering::PartialSeason { .. }
                | Numbering::CompleteSeries
        );
        if self.keys.contains(&key_a) {
            if self.movie
                && let Some(y) = p.year
                && !self.years.is_empty()
                && !self.years.contains(&y)
            {
                return Check::Contradicts(format!(
                    "'{base}' names the year {y}, not {}",
                    self.label
                ));
            }
            if self.movie && tv_numbering {
                return Check::Contradicts(format!(
                    "'{base}' is a TV release, not the movie {}",
                    self.label
                ));
            }
            return Check::Consistent;
        }
        if key_b.is_some_and(|k| self.keys.contains(&k)) {
            return Check::Consistent;
        }
        let explicit = if self.movie {
            p.year.is_some() || tv_numbering
        } else {
            tv_numbering
        };
        if explicit {
            Check::Contradicts(format!(
                "'{base}' names '{}', not {}",
                p.title_tokens.join(" "),
                self.label
            ))
        } else {
            Check::Neutral
        }
    }

    /// The first contradiction among several names.
    pub fn contradiction<'a>(&self, names: impl IntoIterator<Item = &'a str>) -> Option<String> {
        names.into_iter().find_map(|n| match self.check(n) {
            Check::Contradicts(why) => Some(why),
            _ => None,
        })
    }
}

/// The names to judge for a file inside a torrent: the torrent name, each directory and the
/// file itself.
pub fn names_for<'a>(torrent_name: &'a str, rel: &'a str) -> Vec<&'a str> {
    let mut v = vec![torrent_name];
    v.extend(rel.split('/').filter(|c| !c.is_empty()));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series(title: &str, aliases: &[&str]) -> Identity {
        Identity {
            label: title.into(),
            keys: crate::catalog::title_keys(title, aliases),
            years: vec![],
            movie: false,
        }
    }

    fn movie(title: &str, year: u16) -> Identity {
        movie_aka(title, &[], year)
    }

    fn movie_aka(title: &str, aliases: &[&str], year: u16) -> Identity {
        Identity {
            label: format!("{title} ({year})"),
            keys: crate::catalog::title_keys(title, aliases),
            years: vec![year],
            movie: true,
        }
    }

    #[test]
    fn contradictions_neutral_and_consistent_names() {
        let s = series("Parity Partial", &["Parity Partial US"]);
        assert_eq!(
            s.check("Parity.Partial.S01E02.1080p.WEB-DL-UM.mkv"),
            Check::Consistent
        );
        assert_eq!(s.check("Parity Partial US S01E02"), Check::Consistent);
        assert!(matches!(
            s.check("Wrong.Show.S01E02.1080p.WEB-DL-UM.mkv"),
            Check::Contradicts(_)
        ));
        assert!(matches!(s.check("Wrong Show S01"), Check::Contradicts(_)));
        // Obfuscated, partial or structural names are neutral.
        for n in [
            "S01E02.mkv",
            "a1b2c3d4.mkv",
            "Season 1",
            "Sample",
            "episode.mkv",
            "Extras",
        ] {
            assert_eq!(s.check(n), Check::Neutral, "{n}");
        }
        let m = movie("The Matrix", 1999);
        assert_eq!(
            m.check("The.Matrix.1999.1080p.WEB-DL.mkv"),
            Check::Consistent
        );
        assert!(
            matches!(
                m.check("The.Matrix.2021.1080p.WEB-DL.mkv"),
                Check::Contradicts(_)
            ),
            "wrong year"
        );
        assert!(matches!(
            m.check("Other.Movie.2020.1080p.WEB-DL.mkv"),
            Check::Contradicts(_)
        ));
        assert!(matches!(
            m.check("Some.Show.S01E01.1080p.mkv"),
            Check::Contradicts(_)
        ));
        assert_eq!(
            m.check("matrix.mkv"),
            Check::Consistent,
            "same title, no year"
        );
        assert_eq!(m.check("movie-grp.mkv"), Check::Neutral);
        assert!(
            m.contradiction(names_for("The.Matrix.1999.1080p", "Other.Movie.2020.mkv"))
                .is_some()
        );
        assert!(
            m.contradiction(names_for("The.Matrix.1999.1080p", "Sample/sample.mkv"))
                .is_none()
        );
    }

    #[test]
    fn franchise_prefixed_alias_is_the_same_movie_and_nothing_else_is() {
        let m = movie_aka(
            "Nebula Arin: The Last Keeper",
            &[
                "Arin: The Last Keeper",
                "The Legend of Arin: The Last Keeper",
                "Arin – Der letzte Hüter",
            ],
            2026,
        );
        let file =
            "Nebula.The.Legend.of.Arin.The.Last.Keeper.2026.1080p.WEBRip.AAC2.0.x264-GROUP.mp4";
        assert_eq!(m.check(file), Check::Consistent);
        assert_eq!(
            m.check("The.Legend.of.Arin.The.Last.Keeper.2026.1080p.WEBRip.mp4"),
            Check::Consistent
        );
        for wrong in [
            // Another year, a prefix the metadata does not know, a stripped-down subtitle,
            // another movie and a TV episode under the composite title.
            "Nebula.The.Legend.of.Arin.The.Last.Keeper.2025.1080p.WEBRip.mp4",
            "Galaxy.The.Legend.of.Arin.The.Last.Keeper.2026.1080p.WEBRip.mp4",
            "Nebula.The.Legend.of.Arin.2026.1080p.WEBRip.mp4",
            "Nebula.Other.Movie.2026.1080p.WEBRip.mp4",
            "Nebula.The.Legend.of.Arin.The.Last.Keeper.S01E01.1080p.mkv",
        ] {
            assert!(matches!(m.check(wrong), Check::Contradicts(_)), "{wrong}");
        }
        // Obfuscated names stay neutral.
        assert_eq!(m.check("a1b2c3d4.mp4"), Check::Neutral);
        // The same shape for a series: the prefixed long alias is consistent, a foreign
        // prefix still contradicts.
        let s = series("Nebula Arin", &["Arin", "The Legend of Arin"]);
        assert_eq!(
            s.check("Nebula.The.Legend.of.Arin.S01E02.1080p.WEB-DL.mkv"),
            Check::Consistent
        );
        assert!(matches!(
            s.check("Galaxy.The.Legend.of.Arin.S01E02.1080p.WEB-DL.mkv"),
            Check::Contradicts(_)
        ));
    }
}
