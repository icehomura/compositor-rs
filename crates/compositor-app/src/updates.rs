//! Sparkle's replacement: the appcast feed is still published and parsed, the installer is not.
//!
//! `Config/Info.plist`'s `SUFeedURL` points at the appcast `scripts/publish.sh` commits to `main`,
//! and the newest `<item>` carries the released version and the download. Sparkle's installer
//! replaces the macOS `.app` bundle, so it is platform-specific and cannot come along: a newer
//! release opens its link in the browser instead of installing in place. `docs/PORTING.md` § 4
//! records the substitution ("feature preserved as feed check + release link").
//!
//! Sparkle's own alert strings are not in the reference tree, so the "up to date" alert uses
//! Sparkle's default wording from memory and a failed check keeps the project's own error phrasing.

use std::sync::Arc;

use futures::AsyncReadExt as _;
use gpui_kit::http_client::{AsyncBody, HttpClient};

/// `SUFeedURL` from `Config/Info.plist`.
pub const FEED_URL: &str = "https://raw.githubusercontent.com/robbietilton/Compositor/main/appcast.xml";

/// The application's name, as the alerts spell it (`CFBundleName`).
pub const APP_NAME: &str = "Compositor";

/// One release in the feed: the version Sparkle compares, and where it downloads from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    /// `sparkle:shortVersionString`, or the `sparkle:version` build number when the feed omits it.
    pub version: String,
    /// The `<enclosure url>`: the download `scripts/publish.sh` attaches to the GitHub release.
    pub enclosure_url: Option<String>,
    /// The `<link>`: the release's own page.
    pub link: Option<String>,
}

impl Release {
    /// The URL a check opens: the release's download when the feed carries one, its page otherwise.
    pub fn download_url(&self) -> Option<&str> {
        self.enclosure_url.as_deref().or(self.link.as_deref())
    }
}

/// What `Check for Updates…` found (`SPUUpdater`'s two outcomes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateCheck {
    /// A newer release is published; Sparkle shows its notes and an Install button.
    Available(Release),
    /// `updaterDidNotFindUpdate(_:)`: this build is the newest.
    UpToDate,
}

/// `SPUUpdater.checkForUpdates()`: reads the feed and compares its newest item against the running
/// build. An error is Sparkle's "no update check was performed" case, as its message.
pub async fn check(client: Arc<dyn HttpClient>, current_version: &str) -> Result<UpdateCheck, String> {
    let response = client
        .get(FEED_URL, AsyncBody::empty(), true)
        .await
        .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!("The update feed answered {}.", response.status().as_u16()));
    }
    let mut body = String::new();
    response
        .into_body()
        .read_to_string(&mut body)
        .await
        .map_err(|error| error.to_string())?;
    let release = parse_newest(&body).ok_or_else(|| "The update feed carried no release.".to_string())?;
    if is_newer(&release.version, current_version) {
        Ok(UpdateCheck::Available(release))
    } else {
        Ok(UpdateCheck::UpToDate)
    }
}

/// Sparkle's `SUUpToDate` alert: what a check that found nothing shows.
pub fn up_to_date_alert(current_version: &str) -> (String, String) {
    (
        "You're up to date!".to_string(),
        format!("{APP_NAME} {current_version} is currently the newest version available."),
    )
}

/// The feed's newest release. `scripts/publish.sh` writes the appcast newest-first, as Sparkle reads
/// it, so the first `<item>` is the one to compare.
pub fn parse_newest(feed: &str) -> Option<Release> {
    let item = element_body(feed, "item")?;
    let version = element_body(item, "sparkle:shortVersionString").or_else(|| element_body(item, "sparkle:version"))?;
    Some(Release {
        version: version.to_string(),
        enclosure_url: attribute(item, "enclosure", "url"),
        link: element_body(item, "link").map(str::to_string),
    })
}

/// `sparkle:version` order: numeric components compared left to right, a missing one counting as
/// zero, so `1.4.5` beats `1.4` and a suffixed component (`1.5-beta`) counts as its number.
pub fn is_newer(version: &str, current: &str) -> bool {
    let parts = |value: &str| -> Vec<u64> {
        value
            .split(['.', '-', '+'])
            .map(|part| {
                let digits: String = part.chars().take_while(|character| character.is_ascii_digit()).collect();
                digits.parse().unwrap_or(0)
            })
            .collect()
    };
    let (new, running) = (parts(version), parts(current));
    for index in 0..new.len().max(running.len()) {
        let left = new.get(index).copied().unwrap_or(0);
        let right = running.get(index).copied().unwrap_or(0);
        if left != right {
            return left > right;
        }
    }
    false
}

/// The text between the first `<tag>` and its `</tag>`.
fn element_body<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let opening = xml.find(&format!("<{tag}"))?;
    let content = xml[opening..].find('>')? + opening + 1;
    let closing = xml[content..].find(&format!("</{tag}>"))? + content;
    Some(xml[content..closing].trim())
}

/// The value of `attribute` inside the first `<tag …>` element.
fn attribute(xml: &str, tag: &str, attribute: &str) -> Option<String> {
    let opening = xml.find(&format!("<{tag}"))?;
    let end = xml[opening..].find('>')? + opening;
    let element = &xml[opening..end];
    let start = element.find(&format!("{attribute}=\""))? + attribute.len() + 2;
    let close = element[start..].find('"')? + start;
    Some(element[start..close].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The appcast `scripts/publish.sh` writes, as `references/Compositor/appcast.xml` carries it.
    const APPCAST: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0" xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle">
  <channel>
    <title>Compositor</title>
    <item>
      <title>Version 1.4.5</title>
      <pubDate>Tue, 29 Sep 2026 21:42:57 +0000</pubDate>
      <sparkle:version>40</sparkle:version>
      <sparkle:shortVersionString>1.4.5</sparkle:shortVersionString>
      <sparkle:minimumSystemVersion>26.0</sparkle:minimumSystemVersion>
      <link>https://github.com/robbietilton/Compositor/releases/tag/v1.4.5</link>
      <enclosure url="https://github.com/robbietilton/Compositor/releases/download/v1.4.5/Compositor.dmg" sparkle:edSignature="hi5" length="6750067" type="application/octet-stream"/>
    </item>
  </channel>
</rss>
"#;

    #[test]
    fn the_appcast_parses_the_newest_item() {
        let release = parse_newest(APPCAST).expect("an item");
        assert_eq!(release.version, "1.4.5");
        assert_eq!(
            release.download_url(),
            Some("https://github.com/robbietilton/Compositor/releases/download/v1.4.5/Compositor.dmg")
        );
        assert_eq!(release.link.as_deref(), Some("https://github.com/robbietilton/Compositor/releases/tag/v1.4.5"));
    }

    #[test]
    fn versions_compare_numerically_component_by_component() {
        assert!(is_newer("1.4.5", "0.1.0"));
        assert!(is_newer("1.4.10", "1.4.9"));
        assert!(is_newer("1.5", "1.4.5"));
        assert!(is_newer("1.5-beta", "1.4.5"));
        assert!(!is_newer("1.4.5-beta", "1.4.5"));
        assert!(!is_newer("1.4.5", "1.4.5"));
        assert!(!is_newer("1.4", "1.4.0"));
        assert!(!is_newer("0.1.0", "1.4.5"));
    }

    #[test]
    fn a_feed_without_items_has_no_release() {
        assert!(parse_newest("<rss><channel></channel></rss>").is_none());
    }
}
