//! Adversarial matching and policy decisions, evaluated directly (no HTTP).
#![recursion_limit = "256"]

mod common;

use arr::catalog::{AltTitle, CATALOG_SCHEMA_VERSION, Catalog, CatalogIndex, FileEvidence};
use arr::decision::{Decision, EvalContext, Scope, evaluate};
use arr::ids::{MediaId, SeriesKey};
use arr::library::{new_movie, new_series};
use arr::prowlarr::ProwlarrRelease;
use arr::quality::Quality;
use arr::release;
use arr::release::Revision;
use arr::settings::{LanguageRequirement, Settings};
use arr::target::{
    EpisodeInfo, MovieTarget, Ownership, SeasonTarget, SeriesTarget, Source, Target,
};
use common::{ExecOptions, FRANCHISE_ALIASES, FRANCHISE_FILE, managed_settings, release};
use serde_json::{Value, json};
use time::OffsetDateTime;

fn dates() -> arr::catalog::ReleaseDates {
    arr::catalog::ReleaseDates {
        in_cinemas: None,
        physical_release: None,
        digital_release: Some("2000-01-01".into()),
    }
}

/// A managed-catalog movie, as `POST /library` stores it.
fn catalog_movie(tmdb: u32, title: &str, year: u16, aliases: &[&str]) -> arr::catalog::Movie {
    new_movie(
        tmdb,
        title.into(),
        Some(year),
        Some(120),
        Some("english".into()),
        aliases.iter().map(|a| a.to_string()).collect(),
        None,
        Some("setup:movies-hd".into()),
        Some("/movies".into()),
        None,
        dates(),
        "unified-media",
        OffsetDateTime::now_utc(),
    )
}

/// A managed-catalog series, as `POST /library` stores it.
fn catalog_series(tvdb: u32, title: &str, year: u16, aliases: &[&str]) -> arr::catalog::Series {
    let mut s = new_series(
        SeriesKey::Tvdb(tvdb),
        title.into(),
        Some(year),
        Some(50),
        Some("english".into()),
        None,
        Some("setup:tv-hd".into()),
        Some("/tvshows".into()),
        None,
        "unified-media",
        OffsetDateTime::now_utc(),
    );
    s.alternate_titles = aliases
        .iter()
        .map(|t| AltTitle {
            title: (*t).into(),
            season: None,
        })
        .collect();
    s
}

fn catalog_of(movies: Vec<arr::catalog::Movie>, series: Vec<arr::catalog::Series>) -> CatalogIndex {
    CatalogIndex::new(Catalog {
        schema_version: CATALOG_SCHEMA_VERSION,
        import_id: String::new(),
        observed_at: String::new(),
        movies,
        series,
    })
}

/// The harness's standard titles (see `seed_standard_library`).
fn standard_catalog() -> CatalogIndex {
    catalog_of(
        vec![
            catalog_movie(603, "The Matrix", 1999, &[]),
            catalog_movie(604, "The Matrix Reloaded", 2003, &[]),
            catalog_movie(194, "Amélie", 2001, &["Amelie from Montmartre"]),
        ],
        vec![
            catalog_series(372264, "Slow Horses", 2022, &[]),
            catalog_series(269613, "Fargo", 2014, &[]),
        ],
    )
}

fn series(title: &str, tvdb: u32, year: Option<u16>, aliases: Vec<AltTitle>) -> SeriesTarget {
    SeriesTarget {
        id: MediaId::Series(SeriesKey::Tvdb(tvdb)),
        title: title.into(),
        alternate_titles: aliases,
        year,
        runtime_minutes: Some(50),
        original_language: Some("english".into()),
        series_type: "standard".into(),
        scene_numbering: false,
        quality_profile: Some("setup:tv-hd".into()),
        root_folder: Some("/tvshows".into()),
        identity_conflict: false,
        metadata_source: Source::Catalog,
        in_catalog: true,
        tvdb_id: Some(tvdb),
        tvmaze_id: None,
    }
}

fn eps(owned: &[u16], unaired: &[u16], count: u16) -> Vec<EpisodeInfo> {
    (1..=count)
        .map(|n| EpisodeInfo {
            number: n,
            title: None,
            air_date_utc: None,
            aired: !unaired.contains(&n),
            runtime_minutes: Some(50),
            ownership: if owned.contains(&n) {
                Ownership::Owned
            } else {
                Ownership::Missing
            },
            existing: None,
            queued: false,
            replaceable: false,
        })
        .collect()
}

fn season(s: SeriesTarget, n: u16, episodes: Option<Vec<EpisodeInfo>>) -> Target {
    Target::Season(SeasonTarget {
        series: s,
        season: n,
        episodes,
        episode_list_source: Some(Source::Catalog),
        ownership_source: "test".into(),
        notes: vec![],
    })
}

fn slow_horses(owned: &[u16], unaired: &[u16]) -> Target {
    season(
        series("Slow Horses", 372264, Some(2022), vec![]),
        1,
        Some(eps(owned, unaired, 6)),
    )
}

fn movie(title: &str, tmdb: u32, year: u16, ownership: Ownership) -> MovieTarget {
    MovieTarget {
        id: MediaId::Movie { tmdb },
        title: title.into(),
        alternate_titles: vec![],
        year: Some(year),
        secondary_year: None,
        runtime_minutes: Some(136),
        original_language: Some("english".into()),
        is_available: Some(true),
        ownership,
        quality_profile: Some("setup:movies-hd".into()),
        root_folder: None,
        identity_conflict: false,
        metadata_source: Source::Catalog,
        in_catalog: true,
        existing: None,
        queued: false,
        folder: None,
        availability_note: None,
    }
}

struct Env {
    settings: Settings,
    catalog: CatalogIndex,
    blocked: Option<std::collections::HashSet<String>>,
    partial_packs: bool,
}

impl Env {
    /// The managed setup's settings (the default stack's policy; TV takes any hardcoded subs).
    fn new() -> Self {
        Env::managed(ExecOptions {
            tv_extra: "allow_hardcoded_subs = true\n".into(),
            ..Default::default()
        })
    }

    /// As `new`, with revision upgrades of owned files enabled.
    fn upgrading() -> Self {
        Env::managed(ExecOptions {
            tv_extra: "allow_hardcoded_subs = true\nrevision_upgrades = true\n".into(),
            movies_extra: "revision_upgrades = true\n".into(),
            ..Default::default()
        })
    }

    fn managed(o: ExecOptions) -> Self {
        Env {
            settings: managed_settings(&o),
            catalog: standard_catalog(),
            blocked: None,
            partial_packs: false,
        }
    }

    /// As `new`, over a different catalog.
    fn with_catalog(catalog: CatalogIndex) -> Self {
        Env {
            catalog,
            ..Env::new()
        }
    }

    fn eval_release(&self, target: &Target, scope: Scope, r: Value) -> Decision {
        let r: ProwlarrRelease = serde_json::from_value(r).unwrap();
        let p = release::parse(r.title.as_deref().unwrap());
        let profile_id = target.quality_profile().map(str::to_string);
        let ctx = EvalContext {
            target,
            scope,
            settings: &self.settings,
            profile: profile_id
                .as_deref()
                .and_then(|id| self.settings.profile(id)),
            profile_id,
            catalog: Some(&self.catalog),
            now: OffsetDateTime::now_utc(),
            catalog_observed_at: None,
            blocked: self.blocked.as_ref(),
            partial_packs: self.partial_packs,
        };
        let blockers = ctx.target_blockers();
        evaluate(&ctx, &r, &p, &blockers)
    }

    fn eval(&self, target: &Target, scope: Scope, title: &str, gib: f64, seeders: i64) -> Decision {
        self.eval_release(target, scope, release(title, gib, seeders))
    }

    fn codes(&self, target: &Target, scope: Scope, title: &str, gib: f64) -> Vec<String> {
        let d = self.eval(target, scope, title, gib, 10);
        assert_eq!(d.approved, d.rejections.is_empty());
        d.rejections.iter().map(|r| r.code.clone()).collect()
    }
}

const S1: Scope = Scope::Season(1);

#[test]
fn exact_season_scope() {
    let env = Env::new();
    let owned = slow_horses(&[1, 3], &[]);
    let fresh = slow_horses(&[], &[]);
    let d = env.eval(
        &owned,
        S1,
        "Slow.Horses.S01.1080p.ATVP.WEB-DL.DDP5.1.H.264-NTb",
        12.0,
        50,
    );
    assert_eq!(
        d.rejections
            .iter()
            .map(|r| r.code.as_str())
            .collect::<Vec<_>>(),
        ["covers_owned"]
    );
    assert_eq!(d.owned_overlap, [1, 3]);
    assert!(d.full_season);
    assert!(
        env.eval(
            &fresh,
            S1,
            "Slow.Horses.S01.1080p.ATVP.WEB-DL.DDP5.1.H.264-NTb",
            12.0,
            50
        )
        .approved
    );

    for (title, code) in [
        ("Slow.Horses.S01-S04.1080p.WEB-DL", "multi_season_pack"),
        ("Slow.Horses.Season.1-4.1080p.WEB-DL", "multi_season_pack"),
        ("Slow.Horses.S01.S02.1080p.WEB-DL", "multi_season_pack"),
        ("Slow.Horses.S02.1080p.WEB-DL", "wrong_season"),
        ("Slow.Horses.S02E01.1080p.WEB-DL", "wrong_season"),
        ("Slow Horses S01 Part 1 1080p WEB-DL", "partial_season_pack"),
        (
            "Slow.Horses.Complete.Series.1080p.WEB-DL",
            "complete_series_pack",
        ),
        ("Slow.Horses.1080p.WEB-DL", "not_tv_numbered"),
        (
            "Slow.Horses.S01E01.S02E01.1080p.WEB-DL",
            "ambiguous_numbering",
        ),
        ("Slow.Horses.S01E07.1080p.WEB-DL", "unknown_episode"),
        (
            "Slow.Horses.2024.01.15.1080p.WEB-DL",
            "unsupported_numbering",
        ),
    ] {
        assert!(
            env.codes(&fresh, S1, title, 10.0)
                .contains(&code.to_string()),
            "{title}: {:?}",
            env.codes(&fresh, S1, title, 10.0)
        );
    }
    // Multi-episode releases inside the wanted set are fine; overlapping owned ones are not.
    let d = env.eval(&owned, S1, "Slow.Horses.S01E04-E06.1080p.WEB-DL", 6.0, 10);
    assert!(d.approved, "{:?}", d.rejections);
    assert_eq!(d.covered_episodes, [4, 5, 6]);
    let d = env.eval(&owned, S1, "Slow.Horses.S01E02E03.1080p.WEB-DL", 4.0, 10);
    assert_eq!(d.owned_overlap, [3]);
    assert_eq!(d.rejections[0].code, "covers_owned");
}

#[test]
fn airing_ownership_and_episode_list_gaps() {
    let env = Env::new();
    let unaired = slow_horses(&[], &[6]);
    assert!(
        env.codes(&unaired, S1, "Slow.Horses.S01.1080p.WEB-DL", 10.0)
            .contains(&"season_not_fully_aired".into())
    );
    assert!(
        env.codes(&unaired, S1, "Slow.Horses.S01E06.1080p.WEB-DL", 2.0)
            .contains(&"episode_not_aired".into())
    );
    assert!(
        env.eval(&unaired, S1, "Slow.Horses.S01E05.1080p.WEB-DL", 2.0, 10)
            .approved
    );

    let no_list = season(series("Slow Horses", 372264, Some(2022), vec![]), 1, None);
    assert!(
        env.codes(&no_list, S1, "Slow.Horses.S01.1080p.WEB-DL", 10.0)
            .contains(&"episode_list_unknown".into())
    );

    let mut unknown = eps(&[], &[], 6);
    unknown[2].ownership = Ownership::Unknown;
    let unknown = season(
        series("Slow Horses", 372264, Some(2022), vec![]),
        1,
        Some(unknown),
    );
    assert!(
        env.codes(&unknown, S1, "Slow.Horses.S01.1080p.WEB-DL", 10.0)
            .contains(&"ownership_unknown".into())
    );
}

#[test]
fn episode_scope_is_exact() {
    let env = Env::new();
    let t = slow_horses(&[1, 3], &[]);
    let e4 = Scope::Episode(1, 4);
    assert!(
        env.eval(&t, e4, "Slow.Horses.S01E04.1080p.WEB-DL.x264", 2.0, 10)
            .approved
    );
    assert!(
        env.codes(&t, e4, "Slow.Horses.S01E04E05.1080p.WEB-DL", 4.0)
            .contains(&"wrong_episode".into())
    );
    assert!(
        env.codes(&t, e4, "Slow.Horses.S01E05.1080p.WEB-DL", 2.0)
            .contains(&"wrong_episode".into())
    );
    assert!(
        env.codes(&t, e4, "Slow.Horses.S01.1080p.WEB-DL", 10.0)
            .contains(&"season_pack_for_episode".into())
    );
    assert!(
        env.codes(
            &t,
            Scope::Episode(1, 3),
            "Slow.Horses.S01E03.1080p.WEB-DL",
            2.0
        )
        .contains(&"covers_owned".into())
    );
}

#[test]
fn quality_language_size_and_seeders() {
    let env = Env::new();
    let t = slow_horses(&[], &[]);
    let c = |title: &str, gib: f64| env.codes(&t, S1, title, gib);
    assert!(c("Slow.Horses.S01E04.2160p.WEB-DL.HEVC", 8.0).contains(&"quality_not_allowed".into()));
    assert!(
        c("Slow.Horses.S01E04.1080p.BluRay.x264", 4.0).contains(&"quality_not_allowed".into()),
        "Bluray-1080p is disallowed in this profile"
    );
    assert!(c("Slow.Horses.S01E04.WEB-DL.x264", 2.0).contains(&"quality_unknown".into()));
    assert!(
        c("Slow.Horses.S01E04.1080p.WEBRip.BluRay.x264", 2.0).contains(&"quality_unknown".into())
    );
    let inferred = env.eval(&t, S1, "Slow.Horses.S01E04.1080p.x264", 2.0, 10);
    assert!(
        inferred.approved
            && inferred
                .notes
                .iter()
                .any(|n| n.contains("inferred from resolution"))
    );

    // Sonarr v4 applies no language filter without scored custom formats (the retired
    // "Deprecated" language profile in the snapshot is not policy).
    for title in [
        "Slow.Horses.S01E04.FRENCH.1080p.WEB-DL",
        "Slow.Horses.S01E04.MULTi.1080p.WEB-DL",
        "Slow.Horses.S01E04.iTA-ENG.1080p.WEB-DL",
        "Slow.Horses.S01E04.1080p.WEB-DL",
    ] {
        assert!(c(title, 2.0).is_empty(), "{title}: {:?}", c(title, 2.0));
    }
    // Radarr's real profile language (English) stays enforced for movies.
    let m = Target::Movie(movie("The Matrix", 603, 1999, Ownership::Missing));
    assert!(
        env.codes(&m, Scope::Movie, "The.Matrix.1999.FRENCH.1080p.WEB-DL", 8.0)
            .contains(&"language_mismatch".into())
    );
    assert!(
        env.codes(&m, Scope::Movie, "The.Matrix.1999.MULTi.1080p.WEB-DL", 8.0)
            .contains(&"language_ambiguous_multi".into())
    );
    assert!(
        c("Slow.Horses.S01E04.KORSUB.1080p.WEB-DL", 2.0).is_empty(),
        "Sonarr has no hardcoded-subs rule"
    );

    // Sonarr WEBDL-1080p: 4..130 MiB/min x 50 min = 200..6500 MiB per episode; packs scale.
    assert!(c("Slow.Horses.S01E04.1080p.WEB-DL", 0.15).contains(&"size_below_minimum".into()));
    assert!(c("Slow.Horses.S01E04.1080p.WEB-DL", 6.5).contains(&"size_above_maximum".into()));
    assert!(
        c("Slow.Horses.S01E04-E06.1080p.WEB-DL", 6.5).is_empty(),
        "three episodes allow 19500 MiB"
    );
    assert!(
        c("Slow.Horses.S01.1080p.WEB-DL", 1.0).contains(&"size_below_minimum".into()),
        "pack minimum is 1200 MiB"
    );
    let mut no_runtime = eps(&[], &[], 6);
    no_runtime.iter_mut().for_each(|e| e.runtime_minutes = None);
    let mut s = series("Slow Horses", 372264, Some(2022), vec![]);
    s.runtime_minutes = None;
    let t2 = season(s, 1, Some(no_runtime));
    assert!(
        env.codes(&t2, S1, "Slow.Horses.S01E04.1080p.WEB-DL", 2.0)
            .contains(&"size_unverifiable_runtime".into())
    );
    let mut r = release("Slow.Horses.S01E04.1080p.WEB-DL", 2.0, 10);
    r["size"] = json!(0);
    assert!(
        env.eval_release(&t, S1, r)
            .rejections
            .iter()
            .any(|x| x.code == "size_unknown")
    );

    assert_eq!(
        env.eval(&t, S1, "Slow.Horses.S01E04.1080p.WEB-DL", 2.0, 0)
            .rejections[0]
            .code,
        "below_minimum_seeders"
    );
}

#[test]
fn titles_years_aliases_and_ambiguity() {
    let env = Env::new();
    let t = slow_horses(&[], &[]);
    let c = |title: &str| env.codes(&t, S1, title, 2.0);
    assert!(c("Slow.Horses.Revisited.S01E04.1080p.WEB-DL").contains(&"title_mismatch".into()));
    assert!(c("Slow.S01E04.1080p.WEB-DL").contains(&"title_mismatch".into()));
    assert!(c("Horses.S01E04.1080p.WEB-DL").contains(&"title_mismatch".into()));
    assert!(c("SlowHorses.S01E04.1080p.WEB-DL").is_empty());
    assert!(c("Slow.Horses.2022.S01E04.1080p.WEB-DL").is_empty());
    assert!(c("Slow.Horses.2019.S01E04.1080p.WEB-DL").contains(&"year_mismatch".into()));

    // Two catalog series share a title: no year -> ambiguous; the right year disambiguates.
    let env = Env::with_catalog(catalog_of(
        vec![],
        vec![
            catalog_series(372264, "Slow Horses", 2022, &[]),
            catalog_series(73244, "The Office (US)", 2005, &["The Office"]),
            catalog_series(78107, "The Office", 2001, &[]),
        ],
    ));
    let office_us = season(
        series(
            "The Office (US)",
            73244,
            Some(2005),
            vec![AltTitle {
                title: "The Office".into(),
                season: None,
            }],
        ),
        1,
        Some(eps(&[], &[], 6)),
    );
    let codes = env.codes(&office_us, S1, "The.Office.S01E01.720p.HDTV.x264", 0.5);
    assert!(codes.contains(&"ambiguous_title".into()), "{codes:?}");
    assert!(
        env.codes(&office_us, S1, "The.Office.2005.S01E01.720p.HDTV.x264", 0.5)
            .is_empty()
    );
    assert!(
        env.codes(&office_us, S1, "The.Office.US.S01E01.720p.HDTV.x264", 0.5)
            .is_empty(),
        "'(US)' title key matches 'US' token"
    );

    // Unicode titles and aliases; season-scoped alias only applies to its season.
    let s = series(
        "Слово пацана",
        123,
        Some(2023),
        vec![
            AltTitle {
                title: "The Boy's Word".into(),
                season: None,
            },
            AltTitle {
                title: "Pokémon Special".into(),
                season: Some(2),
            },
        ],
    );
    let t1 = season(s.clone(), 1, Some(eps(&[], &[], 6)));
    let t2 = season(s, 2, Some(eps(&[], &[], 6)));
    assert!(
        env.codes(&t1, S1, "Слово.пацана.S01E01.1080p.WEB-DL", 2.0)
            .is_empty()
    );
    assert!(
        env.codes(&t1, S1, "The.Boys.Word.S01E01.1080p.WEB-DL", 2.0)
            .is_empty()
    );
    assert!(
        env.codes(
            &t2,
            Scope::Season(2),
            "Pokemon.Special.S02E01.1080p.WEB-DL",
            2.0
        )
        .is_empty()
    );
    assert!(
        env.codes(&t1, S1, "Pokemon.Special.S01E01.1080p.WEB-DL", 2.0)
            .contains(&"title_mismatch".into())
    );

    // A title word that is also a marker ("Extras") is not treated as one.
    let extras = season(
        series("Extras", 79259, Some(2005), vec![]),
        1,
        Some(eps(&[], &[], 6)),
    );
    assert!(
        env.codes(&extras, S1, "Extras.S01E01.720p.HDTV.x264", 0.5)
            .is_empty()
    );
    assert_eq!(
        env.codes(&extras, S1, "Extras.S01E01.720p.HDTV.x264.Sample", 0.5),
        ["sample_or_extra"],
        "'Sample' is not a title word"
    );
}

#[test]
fn movie_scope_rules() {
    let env = Env::new();
    let m = Target::Movie(movie("The Matrix", 603, 1999, Ownership::Missing));
    let c = |t: &Target, title: &str, gib: f64| env.codes(t, Scope::Movie, title, gib);
    assert!(c(&m, "The.Matrix.1999.1080p.WEB-DL.x264", 8.0).is_empty());
    assert!(
        c(&m, "The.Matrix.Trilogy.1999.1080p.WEB-DL", 8.0).contains(&"multi_movie_bundle".into())
    );
    assert!(
        c(&m, "The.Matrix.1999.Collection.1080p.WEB-DL", 8.0)
            .contains(&"multi_movie_bundle".into())
    );
    assert!(c(&m, "The.Matrix.1999.1080p.WEB-DL.Extras", 8.0).contains(&"sample_or_extra".into()));
    assert!(
        c(&m, "The.Matrix.1999.Behind.The.Scenes.1080p.WEB-DL", 2.0)
            .contains(&"sample_or_extra".into())
    );
    assert!(c(&m, "The.Matrix.1999.3D.HSBS.1080p.BluRay", 8.0).contains(&"three_d".into()));
    assert!(c(&m, "The.Matrix.1999.HC.1080p.WEBRip", 3.0).contains(&"hardcoded_subs".into()));
    assert!(c(&m, "The.Matrix.Reloaded.2003.1080p.WEB-DL", 8.0).contains(&"title_mismatch".into()));

    let owned = Target::Movie(movie("The Matrix", 603, 1999, Ownership::Owned));
    assert!(c(&owned, "The.Matrix.1999.1080p.WEB-DL.x264", 8.0).contains(&"covers_owned".into()));
    let mut unavailable = movie("The Matrix", 603, 1999, Ownership::Missing);
    unavailable.is_available = Some(false);
    assert!(
        c(
            &Target::Movie(unavailable),
            "The.Matrix.1999.1080p.WEB-DL.x264",
            8.0
        )
        .contains(&"not_available".into())
    );
    let mut no_rt = movie("The Matrix", 603, 1999, Ownership::Missing);
    no_rt.runtime_minutes = None;
    assert!(
        c(
            &Target::Movie(no_rt),
            "The.Matrix.1999.1080p.WEB-DL.x264",
            8.0
        )
        .contains(&"size_unverifiable_runtime".into())
    );
    // Bluray-1080p has no maximum in Radarr's definitions, but the profile disallows it anyway.
    assert_eq!(
        c(&m, "The.Matrix.1999.1080p.BluRay.x264", 40.0),
        ["quality_not_allowed"]
    );

    // "The Collection" is a title, not a bundle marker.
    let coll = Target::Movie(movie("The Collection", 120, 2012, Ownership::Missing));
    assert!(c(&coll, "The.Collection.2012.1080p.WEB-DL.x264", 6.0).is_empty());
    // Numeric titles: "1917" (2019) and "Blade Runner 2049" (2017).
    let n1917 = Target::Movie(movie("1917", 530915, 2019, Ownership::Missing));
    assert!(c(&n1917, "1917.2019.1080p.WEB-DL.x264", 6.0).is_empty());
    let br = Target::Movie(movie("Blade Runner 2049", 335984, 2017, Ownership::Missing));
    assert!(c(&br, "Blade.Runner.2049.2017.1080p.WEB-DL.x264", 6.0).is_empty());
    assert!(c(&br, "Blade.Runner.2049.1080p.WEB-DL.x264", 6.0).contains(&"year_missing".into()));
}

#[test]
fn configured_policy_variants() {
    // Global maximum size, torrent delay with top-tier bypass, Original language.
    let tiers = "tiers = [[\"hdtv_720p\"], [\"webdl_720p\", \"webrip_720p\"], [\"bluray_720p\"], [\"hdtv_1080p\"], [\"webdl_1080p\", \"webrip_1080p\"]]\n";
    let env = Env::managed(ExecOptions {
        movies_extra: "maximum_size_mb = 5000\n".into(),
        tv_extra: "torrent_delay_minutes = 60\n".into(),
        profiles: format!(
            "[[profiles]]\nid = \"movies-hd\"\nmedia = \"movie\"\n{tiers}language = \"original\"\nunmarked = \"assume_original\"\n[[profiles]]\nid = \"tv-hd\"\nmedia = \"series\"\n{tiers}"
        ),
        ..Default::default()
    });
    assert_eq!(
        env.settings
            .profile("setup:movies-hd")
            .unwrap()
            .language
            .requirement,
        LanguageRequirement::Original
    );

    let m = Target::Movie(movie("The Matrix", 603, 1999, Ownership::Missing));
    assert!(
        env.codes(&m, Scope::Movie, "The.Matrix.1999.1080p.WEB-DL.x264", 8.0)
            .contains(&"size_above_global_maximum".into())
    );
    let mut french = movie("Amélie", 194, 2001, Ownership::Missing);
    french.original_language = Some("french".into());
    let french = Target::Movie(french);
    assert!(
        env.codes(
            &french,
            Scope::Movie,
            "Amelie.2001.FRENCH.1080p.WEB-DL",
            4.0
        )
        .is_empty(),
        "Original = French"
    );
    assert!(
        env.codes(&french, Scope::Movie, "Amelie.2001.1080p.WEB-DL", 4.0)
            .is_empty(),
        "unmarked = assumed original"
    );
    assert!(
        env.codes(
            &french,
            Scope::Movie,
            "Amelie.2001.ENGLISH.1080p.WEB-DL",
            4.0
        )
        .contains(&"language_mismatch".into())
    );

    let t = slow_horses(&[], &[]);
    let fresh = |title: &str| {
        let mut r = release(title, 2.0, 10);
        r["publishDate"] = json!(arr::import::rfc3339(
            OffsetDateTime::now_utc() - time::Duration::minutes(10)
        ));
        r
    };
    let d = env.eval_release(&t, S1, fresh("Slow.Horses.S01E04.720p.WEBRip.x264"));
    assert!(
        d.rejections.iter().any(|r| r.code == "delay_profile_wait"),
        "{:?}",
        d.rejections
    );
    let d = env.eval_release(&t, S1, fresh("Slow.Horses.S01E04.1080p.WEB-DL.x264"));
    assert!(
        !d.rejections.iter().any(|r| r.code == "delay_profile_wait"),
        "top tier bypasses the delay"
    );
    let mut undated = fresh("Slow.Horses.S01E04.720p.WEBRip.x264");
    undated["publishDate"] = Value::Null;
    assert!(
        env.eval_release(&t, S1, undated)
            .rejections
            .iter()
            .any(|r| r.code == "release_age_unknown")
    );
}

#[test]
fn target_level_blockers() {
    let env = Env::new();
    let mut s = series("Anime Show", 1, Some(2020), vec![]);
    s.series_type = "anime".into();
    let t = season(s, 1, Some(eps(&[], &[], 6)));
    assert!(
        env.codes(&t, S1, "Anime.Show.S01E01.1080p.WEB-DL", 2.0)
            .contains(&"unsupported_series_type".into())
    );
    let mut s = series("Scene Show", 2, Some(2020), vec![]);
    s.scene_numbering = true;
    let t = season(s, 1, Some(eps(&[], &[], 6)));
    assert!(
        env.codes(&t, S1, "Scene.Show.S01E01.1080p.WEB-DL", 2.0)
            .contains(&"unsupported_numbering".into())
    );
    let mut s = series("Dup Show", 3, Some(2020), vec![]);
    s.identity_conflict = true;
    s.quality_profile = Some("sonarr:404".into());
    let t = season(s, 1, Some(eps(&[], &[], 6)));
    let codes = env.codes(&t, S1, "Dup.Show.S01E01.1080p.WEB-DL", 2.0);
    assert!(
        codes.contains(&"identity_conflict".into()) && codes.contains(&"profile_missing".into()),
        "{codes:?}"
    );
}

fn evidence(
    quality: Option<Quality>,
    revision: Option<Revision>,
    path: Option<&str>,
) -> FileEvidence {
    FileEvidence {
        path: path.map(str::to_owned),
        size: Some(1),
        quality,
        revision,
        source: "snapshot".into(),
        observed_at: "2026-01-01T00:00:00Z".into(),
        probe: None,
    }
}

fn rev(version: u8, real: u8) -> Revision {
    Revision { version, real }
}

#[test]
fn revision_upgrades_follow_native_rules() {
    let env = Env::upgrading();
    let owned_movie = |ev: Option<FileEvidence>| {
        let mut m = movie("The Matrix", 603, 1999, Ownership::Owned);
        m.existing = ev;
        Target::Movie(m)
    };
    let web = Some(Quality::Webdl1080);
    let known = owned_movie(Some(evidence(
        web,
        Some(rev(1, 0)),
        Some("The Matrix (1999)/m.mkv"),
    )));
    // Exact quality, higher revision: approved as a replacement although upgrades are off.
    let d = env.eval(
        &known,
        Scope::Movie,
        "The.Matrix.1999.PROPER.1080p.WEB-DL.x264-GRP",
        8.0,
        10,
    );
    assert!(d.approved, "{:?}", d.rejections);
    assert!(d.replacement);
    let d = env.eval(
        &known,
        Scope::Movie,
        "The.Matrix.1999.REPACK2.1080p.WEB-DL.x264-GRP",
        8.0,
        10,
    );
    assert!(d.approved && d.replacement);
    // Same revision, or an ordinary release: blocked.
    let d = env.eval(
        &known,
        Scope::Movie,
        "The.Matrix.1999.1080p.WEB-DL.x264-GRP",
        8.0,
        10,
    );
    assert!(d.rejections.iter().any(|r| r.code == "covers_owned"));
    // Ordinary quality upgrade (different quality) stays blocked even with a higher revision.
    let hd = owned_movie(Some(evidence(
        Some(Quality::Hdtv720),
        Some(rev(1, 0)),
        Some("x/m.mkv"),
    )));
    let d = env.eval(
        &hd,
        Scope::Movie,
        "The.Matrix.1999.PROPER.1080p.WEB-DL.x264-GRP",
        8.0,
        10,
    );
    assert!(
        !d.approved
            && d.rejections
                .iter()
                .any(|r| r.code == "covers_owned" && r.message.contains("exact-quality"))
    );
    // Same group, different quality (WEBRip PROPER over WEB-DL): blocked like native.
    let d = env.eval(
        &known,
        Scope::Movie,
        "The.Matrix.1999.PROPER.1080p.WEBRip.x264-GRP",
        8.0,
        10,
    );
    assert!(!d.approved);
    // Unknown, equal or lower revisions fail closed.
    let unknown = owned_movie(Some(evidence(web, None, Some("x/m.mkv"))));
    let d = env.eval(
        &unknown,
        Scope::Movie,
        "The.Matrix.1999.PROPER.1080p.WEB-DL.x264-GRP",
        8.0,
        10,
    );
    assert!(
        !d.approved
            && d.rejections
                .iter()
                .any(|r| r.message.contains("revision is unknown"))
    );
    let v2 = owned_movie(Some(evidence(web, Some(rev(2, 0)), Some("x/m.mkv"))));
    let d = env.eval(
        &v2,
        Scope::Movie,
        "The.Matrix.1999.PROPER.1080p.WEB-DL.x264-GRP",
        8.0,
        10,
    );
    assert!(!d.approved, "equal revision");
    let v3 = owned_movie(Some(evidence(web, Some(rev(3, 0)), Some("x/m.mkv"))));
    let d = env.eval(
        &v3,
        Scope::Movie,
        "The.Matrix.1999.PROPER.1080p.WEB-DL.x264-GRP",
        8.0,
        10,
    );
    assert!(!d.approved, "lower revision");
    // REAL beats a higher version.
    let d = env.eval(
        &v3,
        Scope::Movie,
        "The.Matrix.1999.REAL.1080p.WEB-DL.x264-GRP",
        8.0,
        10,
    );
    assert!(d.approved && d.replacement, "{:?}", d.rejections);
    // Unknown quality or path also fail closed.
    let no_q = owned_movie(Some(evidence(None, Some(rev(1, 0)), Some("x/m.mkv"))));
    assert!(
        !env.eval(
            &no_q,
            Scope::Movie,
            "The.Matrix.1999.PROPER.1080p.WEB-DL.x264-GRP",
            8.0,
            10
        )
        .approved
    );
    let no_path = owned_movie(Some(evidence(web, Some(rev(1, 0)), None)));
    assert!(
        !env.eval(
            &no_path,
            Scope::Movie,
            "The.Matrix.1999.PROPER.1080p.WEB-DL.x264-GRP",
            8.0,
            10
        )
        .approved
    );
    // A release whose quality is only inferred cannot be compared.
    let d = env.eval(
        &known,
        Scope::Movie,
        "The.Matrix.1999.PROPER.1080p.x264-GRP",
        8.0,
        10,
    );
    assert!(!d.approved);

    // Policy with revision upgrades off (the default): nothing owned is replaced.
    let strict = Env::new();
    let d = strict.eval(
        &known,
        Scope::Movie,
        "The.Matrix.1999.PROPER.1080p.WEB-DL.x264-GRP",
        8.0,
        10,
    );
    assert!(
        !d.approved
            && d.rejections
                .iter()
                .any(|r| r.message.contains("revision upgrades are off"))
    );
}

#[test]
fn queued_and_blocklisted_releases_are_rejected() {
    let mut env = Env::new();
    let mut m = movie("The Matrix", 603, 1999, Ownership::Missing);
    m.queued = true;
    let d = env.eval(
        &Target::Movie(m),
        Scope::Movie,
        "The.Matrix.1999.1080p.WEB-DL.x264-GRP",
        8.0,
        10,
    );
    assert!(d.rejections.iter().any(|r| r.code == "covers_queued"));
    let target = Target::Movie(movie("The Matrix", 603, 1999, Ownership::Missing));
    let r = release("The.Matrix.1999.1080p.WEB-DL.x264-GRP", 8.0, 10);
    let rr: ProwlarrRelease = serde_json::from_value(r.clone()).unwrap();
    env.blocked = Some([arr::search::release_ref(&rr)].into_iter().collect());
    let d = env.eval_release(&target, Scope::Movie, r);
    assert!(d.rejections.iter().any(|r| r.code == "blocklisted"));
    // By info-hash too.
    let r = release("The.Matrix.1999.1080p.WEB-DL.x264-OTHER", 8.0, 10);
    let hash = r["infoHash"].as_str().unwrap().to_string();
    env.blocked = Some([format!("hash:{hash}")].into_iter().collect());
    let d = env.eval_release(&target, Scope::Movie, r);
    assert!(d.rejections.iter().any(|r| r.code == "blocklisted"));
}

#[test]
fn season_pack_replaces_owned_episodes_only_by_revision() {
    let env = Env::upgrading();
    let web = Some(Quality::Webdl1080);
    let mut list = eps(&[1, 2], &[], 6);
    for e in list.iter_mut().filter(|e| e.ownership == Ownership::Owned) {
        e.existing = Some(evidence(
            web,
            Some(rev(1, 0)),
            Some(&format!("Slow Horses/Season 1/e{}.mkv", e.number)),
        ));
    }
    let t = season(
        series("Slow Horses", 372264, Some(2022), vec![]),
        1,
        Some(list.clone()),
    );
    let d = env.eval(
        &t,
        Scope::Season(1),
        "Slow.Horses.S01.PROPER.1080p.WEB-DL.x264-GRP",
        20.0,
        10,
    );
    assert!(d.approved && d.replacement, "{:?}", d.rejections);
    assert_eq!(d.owned_overlap, vec![1, 2]);
    let d = env.eval(
        &t,
        Scope::Season(1),
        "Slow.Horses.S01.1080p.WEB-DL.x264-GRP",
        20.0,
        10,
    );
    assert!(!d.approved && d.rejections.iter().any(|r| r.code == "covers_owned"));
    // One owned episode with unknown revision blocks the pack.
    list[1].existing = Some(evidence(web, None, Some("x.mkv")));
    let t = season(
        series("Slow Horses", 372264, Some(2022), vec![]),
        1,
        Some(list),
    );
    assert!(
        !env.eval(
            &t,
            Scope::Season(1),
            "Slow.Horses.S01.PROPER.1080p.WEB-DL.x264-GRP",
            20.0,
            10
        )
        .approved
    );
}

#[test]
fn franchise_prefixed_alias_is_one_title_for_decisions_and_rss() {
    use arr::rss::{Match, match_item};
    let mut franchise = catalog_movie(
        990001,
        "Nebula Arin: The Last Keeper",
        2026,
        &FRANCHISE_ALIASES,
    );
    franchise.runtime_minutes = Some(110);
    let env = Env::with_catalog(catalog_of(vec![franchise], vec![]));
    let mut m = movie(
        "Nebula Arin: The Last Keeper",
        990001,
        2026,
        Ownership::Missing,
    );
    m.runtime_minutes = Some(110);
    m.alternate_titles = FRANCHISE_ALIASES.iter().map(|t| t.to_string()).collect();
    let t = Target::Movie(m);
    let name = FRANCHISE_FILE.trim_end_matches(".mp4");
    assert_eq!(env.codes(&t, Scope::Movie, name, 4.0), Vec::<String>::new());
    for (wrong, code) in [
        (
            "Nebula.The.Legend.of.Arin.The.Last.Keeper.2025.1080p.WEBRip.AAC2.0.x264-GROUP",
            "year_mismatch",
        ),
        (
            "Galaxy.The.Legend.of.Arin.The.Last.Keeper.2026.1080p.WEBRip.AAC2.0.x264-GROUP",
            "title_mismatch",
        ),
        (
            "Nebula.The.Legend.of.Arin.2026.1080p.WEBRip.x264-GROUP",
            "title_mismatch",
        ),
        (
            "Nebula.Other.Movie.2026.1080p.WEBRip.x264-GROUP",
            "title_mismatch",
        ),
        (
            "Wrong.Show.S01E02.1080p.WEB-DL.x264-GROUP",
            "title_mismatch",
        ),
    ] {
        let codes = env.codes(&t, Scope::Movie, wrong, 4.0);
        assert!(codes.contains(&code.to_string()), "{wrong}: {codes:?}");
    }

    // RSS uses the same keys through the catalog index.
    let item = |title: &str| {
        let mut r = release(title, 4.0, 10);
        r["categories"] = json!([{ "id": 2040, "name": "Movies/HD" }]);
        serde_json::from_value::<ProwlarrRelease>(r).unwrap()
    };
    assert_eq!(
        match_item(&env.catalog, &item(name), &release::parse(name)),
        Match::Movie(MediaId::Movie { tmdb: 990001 })
    );
    for wrong in [
        "Nebula.The.Legend.of.Arin.The.Last.Keeper.2025.1080p.WEBRip.x264-GROUP",
        "Galaxy.The.Legend.of.Arin.The.Last.Keeper.2026.1080p.WEBRip.x264-GROUP",
    ] {
        assert!(
            matches!(
                match_item(&env.catalog, &item(wrong), &release::parse(wrong)),
                Match::Ignored(_)
            ),
            "{wrong}"
        );
    }
}

// ---- season packs that leave episodes out (a client that selects files)

/// A 16-episode season (like Futurama S05) with some episodes owned and some covered by an
/// active download: `(episode, replaceable)`.
fn futurama(owned: &[u16], active: &[(u16, bool)]) -> Target {
    let mut list = eps(owned, &[], 16);
    for (n, replaceable) in active {
        let e = list.iter_mut().find(|e| e.number == *n).unwrap();
        e.queued = true;
        e.replaceable = *replaceable;
    }
    season(series("Futurama", 73871, Some(1999), vec![]), 5, Some(list))
}

const IVY: &str = "Futurama-1999-S05 1080p WEBRip 10bit EAC3 2 0 x265-iVy";

#[test]
fn a_healthy_pack_takes_what_is_missing_and_leaves_the_rest() {
    let mut env = Env::new();
    env.partial_packs = true;
    // E02 is owned. E01 and E07 are idle, E06 is unconfirmed, E11 and E14 are unsent: all
    // replaceable. E09 is downloading at 40%: it keeps its download.
    let target = futurama(
        &[2],
        &[
            (1, true),
            (6, true),
            (7, true),
            (11, true),
            (14, true),
            (9, false),
        ],
    );
    let d = env.eval_release(&target, Scope::Season(5), {
        let mut r = release(IVY, 16.0, 39);
        r["leechers"] = json!(3);
        r
    });
    assert!(d.approved, "{:?}", d.rejections);
    assert_eq!(d.health, arr::decision::Health::Healthy);
    assert!(d.full_season);
    assert_eq!(d.skips_owned, [2]);
    assert_eq!(d.skips_active, [9]);
    assert_eq!(d.supersedes, [1, 6, 7, 11, 14]);
    // The grab gets every episode but the owned one and the one that is downloading.
    assert_eq!(
        d.covered_episodes,
        [1, 3, 4, 5, 6, 7, 8, 10, 11, 12, 13, 14, 15, 16]
    );
    assert!(d.owned_overlap.is_empty() && !d.replacement);
    assert!(
        d.notes
            .iter()
            .any(|n| n.contains("E02") && n.contains("owned")),
        "{:?}",
        d.notes
    );
    // The decision as JSON, for the release list.
    let v = serde_json::to_value(&d).unwrap();
    assert_eq!(v["skips_owned"], json!([2]));
    assert_eq!(v["skips_active"], json!([9]));
    assert_eq!(v["supersedes"], json!([1, 6, 7, 11, 14]));
}

#[test]
fn a_pack_never_displaces_a_download_that_is_progressing_or_not_its_to_replace() {
    let mut env = Env::new();
    env.partial_packs = true;
    // Not replaceable: progressing, paused, user-chosen or another pack all look the same here.
    let target = futurama(&[], &[(3, false), (4, false)]);
    let d = env.eval(&target, Scope::Season(5), IVY, 16.0, 39);
    assert!(d.approved, "{:?}", d.rejections);
    assert_eq!(d.skips_active, [3, 4]);
    assert!(d.supersedes.is_empty());
    assert!(!d.covered_episodes.contains(&3) && !d.covered_episodes.contains(&4));
    // A pack with a weak swarm replaces nothing: replaceable downloads stay too.
    let target = futurama(&[], &[(3, true)]);
    let weak = env.eval(&target, Scope::Season(5), IVY, 16.0, 2);
    assert!(weak.approved, "{:?}", weak.rejections);
    assert_eq!(weak.health, arr::decision::Health::Weak);
    assert!(weak.supersedes.is_empty());
    assert_eq!(weak.skips_active, [3]);
}

#[test]
fn a_pack_with_nothing_left_for_it_is_still_rejected() {
    let mut env = Env::new();
    env.partial_packs = true;
    let all: Vec<u16> = (1..=16).collect();
    let owned = futurama(&all, &[]);
    let d = env.eval(&owned, Scope::Season(5), IVY, 16.0, 39);
    assert_eq!(
        d.rejections
            .iter()
            .map(|r| r.code.as_str())
            .collect::<Vec<_>>(),
        ["covers_owned"]
    );
    // Everything missing is already being downloaded, and progressing.
    let active: Vec<(u16, bool)> = (1..=16).map(|n| (n, false)).collect();
    let busy = futurama(&[], &active);
    let d = env.eval(&busy, Scope::Season(5), IVY, 16.0, 39);
    assert_eq!(
        d.rejections
            .iter()
            .map(|r| r.code.as_str())
            .collect::<Vec<_>>(),
        ["covers_queued"]
    );
    // Single and multi-episode releases of owned episodes keep covers_owned.
    let some_owned = futurama(&[2, 3], &[]);
    let d = env.eval(
        &some_owned,
        Scope::Season(5),
        "Futurama.1999.S05E02E03.1080p.WEBRip.x265-iVy",
        2.0,
        39,
    );
    assert_eq!(d.rejections[0].code, "covers_owned");
}

#[test]
fn without_file_selection_a_pack_must_cover_every_episode_as_before() {
    // The default environment is a client that cannot select files (qBittorrent).
    let env = Env::new();
    let target = futurama(&[2], &[(1, true)]);
    let d = env.eval(&target, Scope::Season(5), IVY, 16.0, 39);
    let codes: Vec<&str> = d.rejections.iter().map(|r| r.code.as_str()).collect();
    assert_eq!(codes, ["covers_queued", "covers_owned"]);
    assert!(d.skips_owned.is_empty() && d.supersedes.is_empty());
    assert_eq!(d.covered_episodes.len(), 16);
}

#[test]
fn a_pack_is_judged_by_size_over_the_whole_season_not_by_what_it_gets() {
    let mut env = Env::new();
    env.partial_packs = true;
    // 14 of 16 episodes owned: the pack still has to be a sane size for 16 episodes.
    let owned: Vec<u16> = (1..=14).collect();
    let target = futurama(&owned, &[]);
    let ok = env.eval(&target, Scope::Season(5), IVY, 16.0, 39);
    assert!(ok.approved, "{:?}", ok.rejections);
    assert_eq!(ok.covered_episodes, [15, 16]);
    let tiny = env.eval(&target, Scope::Season(5), IVY, 0.2, 39);
    assert!(
        tiny.rejections
            .iter()
            .any(|r| r.code == "size_below_minimum"),
        "{:?}",
        tiny.rejections
    );
}

// ---- sport releases stay out of movies and shows

/// Real titles of sport releases (the same ones the sport matcher is tested on): games, fight
/// cards and the shows about them.
const SPORT_TITLES: &[&str] = &[
    "UFC.Fight.Night.293.Cortes-Acosta.vs.Walker.Prelims.1080p.WEB-DL.H264.Fight-BB",
    "UFC.Fight.Night.234.Ankalaev.vs.Walker.2.1080p.WEB-DL.H264.Fight-BB",
    "UFC.333.Volkanovski.vs.Evloev.1080p.WEB-DL.H264-nVa",
    "UFC.333.Prelims.1080p.WEB-DL.H264.Fight-BB",
    "UFC 333 Volkanovski Vs Evloev 1080P 60FPS Full Event WEB-DL AAC2.0. X265 POOTLED",
    "PFL Tampa Cyborg vs Vieira 2026 08 22 720p WEB-DL H264-FBB",
    "PFL MENA 9 2026 720p WEB H 264-JFF",
    "One Championship ONE Fight Night 49 60fps 1080p WEBRip h264-TJ [TJET]",
    "UFC 334 Embedded-Vlog Series-Episode 2 1080p WEBRip h264-TJ [TJET]",
    "UFC 334 Countdown 1080p WEBRip h264-TJ [TJET]",
    "A-League 2026 11 07 Melbourne City Vs Western Sydney Wanderers 1080p HDTV H264-DARKSPORT",
    "Frauen-Bundesliga 2026 11 09 Koln vs Leipzig 720p WEB h264-ViCEROYS",
    "EPL.2019.01.19.Liverpool.vs.Crystal.Palace.XviD-AFG",
    "EPL - Liverpool vs Crystal Palace 19.01.2019 720p HD",
    "Match Of The Day 2026 11 08 1080p HEVC x265-MeGusta",
    "Rugby League Super League Wigan vs Wakefield 26 Sep 2026 BBC 720p H.264-teaspoon",
    "BBC.Women's.Six.Nations.2026.05.17.Ireland.v.Scotland.1080i.HDTV.h264-NX",
    "NBA 2K27 Update v2 1-RUNE",
    "PGA TOUR 2K25 (v2026.03.27 & All DLCs) (Hypervisor|DenuvOwO)",
];

#[test]
fn sport_releases_are_still_refused_for_movies_and_shows() {
    use arr::rss::{Match, match_item};
    // A catalog that holds titles named like the sport: a show called UFC, a series of
    // league shows and a film with a card's name. A sport release must not be taken for any.
    let env = Env::with_catalog(catalog_of(
        vec![
            catalog_movie(990101, "UFC 333", 2026, &[]),
            catalog_movie(990102, "UFC Fight Night", 2026, &[]),
        ],
        vec![
            catalog_series(990201, "UFC", 2026, &[]),
            catalog_series(990202, "A-League", 2026, &[]),
            catalog_series(990203, "EPL", 2019, &[]),
            catalog_series(990204, "Match of the Day", 1964, &[]),
        ],
    ));
    let shows = [
        season(
            series("UFC", 990201, Some(2026), vec![]),
            1,
            Some(eps(&[], &[], 10)),
        ),
        season(
            series("A-League", 990202, Some(2026), vec![]),
            1,
            Some(eps(&[], &[], 10)),
        ),
        season(
            series("EPL", 990203, Some(2019), vec![]),
            1,
            Some(eps(&[], &[], 10)),
        ),
        season(
            series("Match of the Day", 990204, Some(1964), vec![]),
            1,
            Some(eps(&[], &[], 10)),
        ),
    ];
    let films = [
        Target::Movie(movie("UFC 333", 990101, 2026, Ownership::Missing)),
        Target::Movie(movie("UFC Fight Night", 990102, 2026, Ownership::Missing)),
    ];
    // The harness is not vacuous: a proper release of the same show or film is approved.
    assert!(
        env.eval(&shows[0], S1, "UFC.S01.1080p.WEB-DL.x264", 2.0, 30)
            .approved
    );
    assert!(
        env.eval(
            &films[0],
            Scope::Movie,
            "UFC.333.2026.1080p.WEB-DL.x264",
            4.0,
            30
        )
        .approved
    );
    for title in SPORT_TITLES {
        for show in &shows {
            let d = env.eval(show, S1, title, 2.0, 30);
            assert!(!d.approved, "{title} approved for a show: {d:?}");
        }
        for film in &films {
            let d = env.eval(film, Scope::Movie, title, 4.0, 30);
            assert!(!d.approved, "{title} approved for a film: {d:?}");
        }
        // RSS matching, in a TV category and in a movie category.
        for category in [
            json!([{ "id": 5040, "name": "TV/HD" }]),
            json!([{ "id": 2040, "name": "Movies/HD" }]),
        ] {
            let mut r = release(title, 2.0, 30);
            r["categories"] = category;
            let r: ProwlarrRelease = serde_json::from_value(r).unwrap();
            assert!(
                matches!(
                    match_item(&env.catalog, &r, &release::parse(title)),
                    Match::Ignored(_)
                ),
                "{title} matched a title in the catalog"
            );
        }
    }
}
