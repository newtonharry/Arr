//! Adversarial matching and policy decisions, evaluated directly (no HTTP).
#![recursion_limit = "256"]

mod common;

use arr::catalog::{AltTitle, CatalogIndex, FileEvidence};
use arr::decision::{Decision, EvalContext, Scope, evaluate};
use arr::ids::{MediaId, SeriesKey};
use arr::import;
use arr::prowlarr::ProwlarrRelease;
use arr::quality::Quality;
use arr::release;
use arr::release::Revision;
use arr::settings::{LanguageRequirement, Settings};
use arr::target::{
    EpisodeInfo, MovieTarget, Ownership, SeasonTarget, SeriesTarget, Source, Target,
};
use common::{
    ExecOptions, FRANCHISE_ALIASES, FRANCHISE_FILE, managed_settings, release, snapshot,
    with_franchise_movie,
};
use serde_json::{Value, json};
use time::OffsetDateTime;

fn load(v: &Value) -> (Settings, CatalogIndex) {
    let n = import::normalize(
        v,
        "imp-test",
        "2026-10-01T12:00:00Z",
        "sha",
        0,
        "2026-10-01T12:00:00Z",
    )
    .unwrap();
    (n.settings, CatalogIndex::new(n.catalog))
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
    /// Settings from a Sonarr/Radarr snapshot (`with`): the targets' managed profile ids are
    /// mapped to the snapshot's.
    snapshot_profiles: bool,
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
        let (_, catalog) = load(&snapshot());
        Env {
            settings: managed_settings(&o),
            catalog,
            blocked: None,
            snapshot_profiles: false,
        }
    }

    fn with(v: &Value) -> Self {
        let (settings, catalog) = load(v);
        Env {
            settings,
            catalog,
            blocked: None,
            snapshot_profiles: true,
        }
    }

    fn eval_release(&self, target: &Target, scope: Scope, r: Value) -> Decision {
        let r: ProwlarrRelease = serde_json::from_value(r).unwrap();
        let p = release::parse(r.title.as_deref().unwrap());
        let profile_id = target
            .quality_profile()
            .map(|p| match (self.snapshot_profiles, p) {
                (true, "setup:movies-hd") => "radarr:6".to_string(),
                (true, "setup:tv-hd") => "sonarr:6".to_string(),
                _ => p.to_string(),
            });
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
    let mut v = snapshot();
    let mut us = v["sonarr"]["series"][0].clone();
    us["id"] = json!(30);
    us["title"] = json!("The Office (US)");
    us["tvdbId"] = json!(73244);
    us["year"] = json!(2005);
    us["alternateTitles"] = json!([{ "title": "The Office", "seasonNumber": -1 }]);
    let mut uk = us.clone();
    uk["id"] = json!(31);
    uk["title"] = json!("The Office");
    uk["tvdbId"] = json!(78107);
    uk["year"] = json!(2001);
    uk["alternateTitles"] = json!([]);
    v["sonarr"]["series"]
        .as_array_mut()
        .unwrap()
        .extend([us, uk]);
    let env = Env::with(&v);
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
fn imported_policy_variants() {
    // Global maximum size, torrent delay with top-tier bypass, required flags, Original language.
    let mut v = snapshot();
    v["radarr"]["config/indexer"]["maximumSize"] = json!(5000);
    v["sonarr"]["delayprofile"][0]["torrentDelay"] = json!(60);
    v["sonarr"]["indexer"][0]["fields"][7]["value"] = json!([1]);
    v["radarr"]["qualityprofile"][0]["language"] = json!({ "id": -2, "name": "Original" });
    let env = Env::with(&v);
    assert_eq!(
        env.settings
            .profile("radarr:6")
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
    assert!(
        d.rejections
            .iter()
            .any(|r| r.code == "indexer_policy_unsupported"),
        "required flags are not evaluated"
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

    // Policy without preferAndUpgrade: no revision upgrades.
    let mut snap = snapshot();
    snap["radarr"]["config/mediamanagement"]["downloadPropersAndRepacks"] =
        json!("preferAndUpgrade".replace("AndUpgrade", ""));
    let strict = Env::with(&snap);
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
                .any(|r| r.message.contains("preferAndUpgrade"))
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
    let env = Env::with(&with_franchise_movie(snapshot()));
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
