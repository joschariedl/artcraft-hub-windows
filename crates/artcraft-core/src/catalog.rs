//! The ArtCraft "Craft Apps" catalogue.
//!
//! The list mirrors <https://getartcraft.com/apps>. Every official app lives
//! under the `storytold` GitHub organisation and ships a `linux-x86_64.tar.gz`
//! asset plus a `SHA256SUMS.txt` in each release.

/// One Craft App.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppDef {
    /// Repository/binary/asset stem, e.g. `photocraft`.
    pub id: &'static str,
    /// Display name, e.g. `PhotoCraft`.
    pub name: &'static str,
    /// Short German description for the hub.
    pub tagline: &'static str,
    /// Brand colour (sRGB) from getartcraft.com.
    pub color: [u8; 3],
    /// GitHub `owner/repo`.
    pub repo: &'static str,
    /// Older asset stems (PdfCraft shipped as PrintCraft until 0.2.1).
    pub stems: &'static [&'static str],
}

impl AppDef {
    /// Desktop file id, Flatpak id and icon name, e.g. `ai.storyteller.photocraft`.
    pub fn bundle_id(&self) -> String {
        format!("ai.storyteller.{}", self.id)
    }

    pub fn website(&self) -> String {
        format!("https://getartcraft.com/apps/{}", self.id)
    }

    pub fn releases_url(&self) -> String {
        format!("https://github.com/{}/releases", self.repo)
    }

    /// Candidate names of the 1024 px PNG inside the app's repository.
    pub fn icon_paths(&self) -> Vec<String> {
        let mut out = Vec::new();
        for stem in std::iter::once(self.id).chain(self.stems.iter().copied()) {
            out.push(format!("assets/app-icon/{stem}-1024.png"));
            out.push(format!("assets/app-icon/hicolor/1024x1024/apps/ai.storyteller.{stem}.png"));
            out.push(format!("assets/{stem}-1024.png"));
            out.push(format!("packaging/linux/{stem}-1024.png"));
        }
        out
    }
}

const fn app(
    id: &'static str,
    name: &'static str,
    tagline: &'static str,
    color: [u8; 3],
    repo: &'static str,
) -> AppDef {
    AppDef {
        id,
        name,
        tagline,
        color,
        repo,
        stems: &[],
    }
}

/// All 15 apps from getartcraft.com/apps that ArtCraft Hub supports, in website order.
pub const SUITE: [AppDef; 15] = [
    app("photocraft", "PhotoCraft", "Image editing", [0x2f, 0x7b, 0xf5], "storytold/photocraft"),
    app("vectorcraft", "VectorCraft", "Vector illustration", [0xe8, 0x57, 0x3f], "storytold/vectorcraft"),
    app("filmcraft", "FilmCraft", "Video editing, colour and sound", [0x8b, 0x5c, 0xf6], "storytold/filmcraft"),
    app("lightcraft", "LightCraft", "Photo library and RAW development", [0xf2, 0xa5, 0x16], "storytold/lightcraft"),
    AppDef {
        stems: &["printcraft"],
        ..app("pdfcraft", "PdfCraft", "Read, organise and protect PDFs", [0xe8, 0x20, 0x4f], "storytold/pdfcraft")
    },
    app("effectcraft", "EffectCraft", "Motion graphics and VFX", [0xe0, 0x36, 0x8f], "storytold/effectcraft"),
    app("designcraft", "DesignCraft", "Page layout and publishing", [0x7b, 0xb5, 0x1c], "storytold/designcraft"),
    app("figcraft", "FigCraft", "Interface design and prototyping", [0x02, 0xda, 0x96], "storytold/figcraft"),
    app("texturecraft", "TextureCraft", "3D texture painting", [0xc2, 0x67, 0x2b], "storytold/texturecraft"),
    app("materialcraft", "MaterialCraft", "Procedural materials", [0x17, 0x8a, 0x9c], "storytold/materialcraft"),
    app("wordcraft", "WordCraft", "Word processing and document design", [0x3f, 0x5f, 0xe0], "storytold/wordcraft"),
    app("soundcraft", "SoundCraft", "Record, edit and mix audio", [0x1f, 0xa5, 0xc9], "storytold/soundcraft"),
    app("deckcraft", "DeckCraft", "Presentations and slideshows", [0xf0, 0x7a, 0x1e], "storytold/deckcraft"),
    app("cadcraft", "CADCraft", "CAD and technical drawing", [0x3e, 0xa6, 0xe8], "storytold/cadcraft"),
    app("gridcraft", "GridCraft", "Spreadsheets and data analysis", [0x1f, 0x9d, 0x55], "storytold/gridcraft"),
];

/// Look up an app by id (case-insensitive).
pub fn by_id(id: &str) -> Option<&'static AppDef> {
    let id = id.trim().to_ascii_lowercase();
    SUITE.iter().find(|a| a.id.eq_ignore_ascii_case(&id))
}

/// The architecture token used in release asset names.
pub fn release_arch() -> &'static str {
    std::env::consts::ARCH
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_resolvable() {
        for (i, a) in SUITE.iter().enumerate() {
            assert!(by_id(a.id).is_some());
            assert!(!SUITE[..i].iter().any(|b| b.id == a.id));
            assert_eq!(a.bundle_id(), format!("ai.storyteller.{}", a.id));
        }
    }

    #[test]
    fn every_app_has_an_icon_candidate() {
        for a in SUITE.iter() {
            assert!(
                a.icon_paths()
                    .iter()
                    .any(|p| p == &format!("assets/app-icon/{}-1024.png", a.id)),
                "{} lacks its 1024 px icon path",
                a.id
            );
        }
    }
}
