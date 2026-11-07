//! Unified quality identifiers shared by Radarr- and Sonarr-derived profiles.
//!
//! Native quality ids differ between the two applications (Sonarr 20 is "Bluray-1080p
//! Remux", Radarr 30 is "Remux-1080p"), so profiles are imported by native *name*.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Quality {
    #[serde(rename = "unknown")]
    Unknown,
    #[serde(rename = "workprint")]
    Workprint,
    #[serde(rename = "cam")]
    Cam,
    #[serde(rename = "telesync")]
    Telesync,
    #[serde(rename = "telecine")]
    Telecine,
    #[serde(rename = "regional")]
    Regional,
    #[serde(rename = "dvdscr")]
    Dvdscr,
    #[serde(rename = "sdtv")]
    Sdtv,
    #[serde(rename = "dvd")]
    Dvd,
    #[serde(rename = "dvdr")]
    Dvdr,
    #[serde(rename = "webdl_480p")]
    Webdl480,
    #[serde(rename = "webrip_480p")]
    Webrip480,
    #[serde(rename = "bluray_480p")]
    Bluray480,
    #[serde(rename = "bluray_576p")]
    Bluray576,
    #[serde(rename = "hdtv_720p")]
    Hdtv720,
    #[serde(rename = "webdl_720p")]
    Webdl720,
    #[serde(rename = "webrip_720p")]
    Webrip720,
    #[serde(rename = "bluray_720p")]
    Bluray720,
    #[serde(rename = "hdtv_1080p")]
    Hdtv1080,
    #[serde(rename = "webdl_1080p")]
    Webdl1080,
    #[serde(rename = "webrip_1080p")]
    Webrip1080,
    #[serde(rename = "bluray_1080p")]
    Bluray1080,
    #[serde(rename = "remux_1080p")]
    Remux1080,
    #[serde(rename = "hdtv_2160p")]
    Hdtv2160,
    #[serde(rename = "webdl_2160p")]
    Webdl2160,
    #[serde(rename = "webrip_2160p")]
    Webrip2160,
    #[serde(rename = "bluray_2160p")]
    Bluray2160,
    #[serde(rename = "remux_2160p")]
    Remux2160,
    #[serde(rename = "brdisk")]
    Brdisk,
    #[serde(rename = "rawhd")]
    Rawhd,
}

impl Quality {
    /// Every quality, in declaration order.
    pub const ALL: [Quality; 30] = {
        use Quality::*;
        [
            Unknown, Workprint, Cam, Telesync, Telecine, Regional, Dvdscr, Sdtv, Dvd, Dvdr,
            Webdl480, Webrip480, Bluray480, Bluray576, Hdtv720, Webdl720, Webrip720, Bluray720,
            Hdtv1080, Webdl1080, Webrip1080, Bluray1080, Remux1080, Hdtv2160, Webdl2160,
            Webrip2160, Bluray2160, Remux2160, Brdisk, Rawhd,
        ]
    };

    /// Maps a Radarr v5 or Sonarr v4 quality name. Unknown names return `None` so the
    /// importer can report them instead of guessing.
    pub fn from_native_name(name: &str) -> Option<Quality> {
        use Quality::*;
        let q = match name.trim().to_ascii_lowercase().as_str() {
            "unknown" => Unknown,
            "workprint" => Workprint,
            "cam" => Cam,
            "telesync" => Telesync,
            "telecine" => Telecine,
            "regional" => Regional,
            "dvdscr" => Dvdscr,
            "sdtv" => Sdtv,
            "dvd" => Dvd,
            "dvd-r" => Dvdr,
            "webdl-480p" => Webdl480,
            "webrip-480p" => Webrip480,
            "bluray-480p" => Bluray480,
            "bluray-576p" => Bluray576,
            "hdtv-720p" => Hdtv720,
            "webdl-720p" => Webdl720,
            "webrip-720p" => Webrip720,
            "bluray-720p" => Bluray720,
            "hdtv-1080p" => Hdtv1080,
            "webdl-1080p" => Webdl1080,
            "webrip-1080p" => Webrip1080,
            "bluray-1080p" => Bluray1080,
            "remux-1080p" | "bluray-1080p remux" => Remux1080,
            "hdtv-2160p" => Hdtv2160,
            "webdl-2160p" => Webdl2160,
            "webrip-2160p" => Webrip2160,
            "bluray-2160p" => Bluray2160,
            "remux-2160p" | "bluray-2160p remux" => Remux2160,
            "br-disk" => Brdisk,
            "raw-hd" => Rawhd,
            _ => return None,
        };
        Some(q)
    }

    /// Serialized name (kept in sync with the serde renames by a unit test).
    pub fn as_str(self) -> &'static str {
        QUALITY_NAMES[self as usize]
    }

    /// Nominal resolution of a resolution-specific quality (`None` for unknown, pre-release,
    /// BR-DISK and Raw-HD qualities).
    pub fn resolution(self) -> Option<u16> {
        use Quality::*;
        Some(match self {
            Sdtv | Dvd | Dvdr | Webdl480 | Webrip480 | Bluray480 => 480,
            Bluray576 => 576,
            Hdtv720 | Webdl720 | Webrip720 | Bluray720 => 720,
            Hdtv1080 | Webdl1080 | Webrip1080 | Bluray1080 | Remux1080 => 1080,
            Hdtv2160 | Webdl2160 | Webrip2160 | Bluray2160 | Remux2160 => 2160,
            _ => return None,
        })
    }

    /// The same source at another resolution, if such a quality exists.
    pub fn with_resolution(self, res: u16) -> Option<Quality> {
        use Quality::*;
        let sd = |r: u16| r <= 576;
        Some(match (self, res) {
            (Webdl480 | Webdl720 | Webdl1080 | Webdl2160, r) if sd(r) => Webdl480,
            (Webdl480 | Webdl720 | Webdl1080 | Webdl2160, 720) => Webdl720,
            (Webdl480 | Webdl720 | Webdl1080 | Webdl2160, 1080) => Webdl1080,
            (Webdl480 | Webdl720 | Webdl1080 | Webdl2160, 2160) => Webdl2160,
            (Webrip480 | Webrip720 | Webrip1080 | Webrip2160, r) if sd(r) => Webrip480,
            (Webrip480 | Webrip720 | Webrip1080 | Webrip2160, 720) => Webrip720,
            (Webrip480 | Webrip720 | Webrip1080 | Webrip2160, 1080) => Webrip1080,
            (Webrip480 | Webrip720 | Webrip1080 | Webrip2160, 2160) => Webrip2160,
            (Bluray480 | Bluray576 | Bluray720 | Bluray1080 | Bluray2160, 480) => Bluray480,
            (Bluray480 | Bluray576 | Bluray720 | Bluray1080 | Bluray2160, 576) => Bluray576,
            (Bluray480 | Bluray576 | Bluray720 | Bluray1080 | Bluray2160, 720) => Bluray720,
            (Bluray480 | Bluray576 | Bluray720 | Bluray1080 | Bluray2160, 1080) => Bluray1080,
            (Bluray480 | Bluray576 | Bluray720 | Bluray1080 | Bluray2160, 2160) => Bluray2160,
            (Remux1080 | Remux2160, 1080) => Remux1080,
            (Remux1080 | Remux2160, 2160) => Remux2160,
            (Sdtv | Hdtv720 | Hdtv1080 | Hdtv2160, r) if sd(r) => Sdtv,
            (Sdtv | Hdtv720 | Hdtv1080 | Hdtv2160, 720) => Hdtv720,
            (Sdtv | Hdtv720 | Hdtv1080 | Hdtv2160, 1080) => Hdtv1080,
            (Sdtv | Hdtv720 | Hdtv1080 | Hdtv2160, 2160) => Hdtv2160,
            (Dvd, r) if sd(r) => Dvd,
            (Dvdr, r) if sd(r) => Dvdr,
            _ => return None,
        })
    }
}

/// Resolution from video dimensions with the native MediaInfo thresholds (width **or**
/// height, so cropped widescreen keeps its class): Sonarr v4.0.15
/// `AugmentQualityFromMediaInfo`.
pub fn resolution_from_dimensions(width: u32, height: u32) -> Option<u16> {
    Some(match (width, height) {
        (w, h) if w >= 3200 || h >= 2100 => 2160,
        (w, h) if w >= 1800 || h >= 1000 => 1080,
        (w, h) if w >= 1200 || h >= 700 => 720,
        (w, h) if w >= 1000 || h >= 560 => 576,
        (w, h) if w > 0 || h > 0 => 480,
        _ => return None,
    })
}

/// Effective file quality: the release's source, with the resolution measured from the file
/// (media info outranks name tags, as natively). `Err` when the measured resolution does not
/// exist for that source or nothing is known.
pub fn effective(advertised: Option<Quality>, width: u32, height: u32) -> Result<Quality, String> {
    let q = advertised.ok_or("the release quality is unknown")?;
    let res = resolution_from_dimensions(width, height).ok_or("the video has no dimensions")?;
    match q.resolution() {
        None => Ok(q),
        Some(r) if r == res || (r <= 576 && res <= 576 && q.with_resolution(res) == Some(q)) => {
            Ok(q)
        }
        Some(_) => q
            .with_resolution(res)
            .ok_or_else(|| format!("{width}x{height} ({res}p) does not exist for a {q} source")),
    }
}

/// Indexed by discriminant, in declaration order.
const QUALITY_NAMES: [&str; 30] = [
    "unknown",
    "workprint",
    "cam",
    "telesync",
    "telecine",
    "regional",
    "dvdscr",
    "sdtv",
    "dvd",
    "dvdr",
    "webdl_480p",
    "webrip_480p",
    "bluray_480p",
    "bluray_576p",
    "hdtv_720p",
    "webdl_720p",
    "webrip_720p",
    "bluray_720p",
    "hdtv_1080p",
    "webdl_1080p",
    "webrip_1080p",
    "bluray_1080p",
    "remux_1080p",
    "hdtv_2160p",
    "webdl_2160p",
    "webrip_2160p",
    "bluray_2160p",
    "remux_2160p",
    "brdisk",
    "rawhd",
];

impl std::fmt::Display for Quality {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_match_serde() {
        for (i, q) in Quality::ALL.iter().enumerate() {
            assert_eq!(*q as usize, i, "ALL is in declaration order");
        }
        for (i, name) in QUALITY_NAMES.iter().enumerate() {
            let q: Quality = serde_json::from_value(serde_json::json!(name)).unwrap();
            assert_eq!(q as usize, i);
            assert_eq!(serde_json::to_value(q).unwrap(), serde_json::json!(name));
        }
    }

    #[test]
    fn media_info_resolution_and_effective_quality() {
        assert_eq!(resolution_from_dimensions(3840, 2160), Some(2160));
        assert_eq!(resolution_from_dimensions(1920, 1080), Some(1080));
        assert_eq!(
            resolution_from_dimensions(1920, 800),
            Some(1080),
            "cropped widescreen stays HD"
        );
        assert_eq!(resolution_from_dimensions(1280, 536), Some(720));
        assert_eq!(
            resolution_from_dimensions(1440, 1080),
            Some(1080),
            "4:3 HD by height"
        );
        assert_eq!(resolution_from_dimensions(720, 576), Some(576));
        assert_eq!(resolution_from_dimensions(640, 360), Some(480));
        assert_eq!(resolution_from_dimensions(0, 0), None);
        let w = Some(Quality::Webdl1080);
        assert_eq!(effective(w, 1920, 1080), Ok(Quality::Webdl1080));
        assert_eq!(effective(w, 3840, 2160), Ok(Quality::Webdl2160));
        assert_eq!(effective(w, 1280, 720), Ok(Quality::Webdl720));
        assert_eq!(
            effective(Some(Quality::Hdtv720), 1920, 1080),
            Ok(Quality::Hdtv1080)
        );
        assert_eq!(effective(Some(Quality::Dvd), 720, 576), Ok(Quality::Dvd));
        assert!(effective(Some(Quality::Dvd), 1920, 1080).is_err());
        assert!(effective(Some(Quality::Remux1080), 1280, 720).is_err());
        assert!(effective(None, 1920, 1080).is_err());
    }

    #[test]
    fn maps_both_native_naming_schemes() {
        assert_eq!(
            Quality::from_native_name("Remux-1080p"),
            Some(Quality::Remux1080)
        );
        assert_eq!(
            Quality::from_native_name("Bluray-1080p Remux"),
            Some(Quality::Remux1080)
        );
        assert_eq!(
            Quality::from_native_name("WEBDL-1080p"),
            Some(Quality::Webdl1080)
        );
        assert_eq!(Quality::from_native_name("BR-DISK"), Some(Quality::Brdisk));
        assert_eq!(Quality::from_native_name("Bluray-8K"), None);
    }
}
