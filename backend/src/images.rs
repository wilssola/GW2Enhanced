//! Images of the site (`site/img/`), embedded in the Worker and served as
//! `/img/{name}`.
//!
//! To add one: put the file in `site/img/`, add a line to [`IMAGES`] and an
//! entry to `GALLERY` in `site/app.js`. The tests below fail if the three
//! disagree. Keep each file small (a few hundred KB at most, JPEG or WebP):
//! everything here ends up inside the Worker bundle.

macro_rules! img {
    ($name:literal) => {
        ($name, include_bytes!(concat!("../site/img/", $name)) as &[u8])
    };
}

pub const IMAGES: &[(&str, &[u8])] = &[
    img!("hd-wall.jpg"),
    img!("hd-weapon.jpg"),
    img!("hd-building.jpg"),
    img!("hd-city.jpg"),
    img!("hd-armor-leaves.jpg"),
    img!("hd-armor-trim.jpg"),
    img!("hd-creature.jpg"),
    img!("hd-fur.jpg"),
    img!("hd-leaf.jpg"),
    img!("hd-wood.jpg"),
    img!("hd-flower.jpg"),
    img!("hd-crystal-blade.jpg"),
    img!("hd-engraving.jpg"),
    img!("hd-blade.jpg"),
];

/// The bytes and content type of `/img/{name}`, if it exists.
pub fn image(name: &str) -> Option<(&'static [u8], &'static str)> {
    let (_, bytes) = IMAGES.iter().find(|(n, _)| *n == name)?;
    Some((*bytes, content_type(name)?))
}

fn content_type(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1;
    Some(match ext {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "avif" => "image/avif",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn listed() -> BTreeSet<String> {
        IMAGES.iter().map(|(n, _)| n.to_string()).collect()
    }

    #[test]
    fn every_file_in_the_folder_is_listed_with_a_known_type() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/site/img");
        let files: BTreeSet<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(files, listed(), "site/img/ and IMAGES differ");
        for (n, b) in IMAGES {
            assert!(content_type(n).is_some(), "{n}: unknown image type");
            assert!(b.len() < 1024 * 1024, "{n}: over 1 MB");
        }
    }

    #[test]
    fn every_image_the_site_uses_exists() {
        let js = include_str!("../site/app.js");
        let html = include_str!("../site/index.html");
        let used: BTreeSet<String> = [js, html]
            .iter()
            .flat_map(|s| s.split("/img/").skip(1))
            .map(|rest| {
                rest.split(|c: char| c == '"' || c == '\'' || c == ')' || c.is_whitespace())
                    .next()
                    .unwrap()
                    .to_string()
            })
            .filter(|n| n.contains('.'))
            .collect();
        for u in &used {
            assert!(image(u).is_some(), "/img/{u} is used by the site but not served");
        }
        assert_eq!(&used, &listed(), "an image is served but not shown");
    }

    #[test]
    fn unknown_names_are_not_served() {
        assert!(image("nope.jpg").is_none());
        assert!(image("../app.js").is_none());
        assert_eq!(image("hd-wall.jpg").unwrap().1, "image/jpeg");
    }
}
