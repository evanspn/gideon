//! Minimal `ComicInfo.xml` metadata parsing.
//!
//! ComicInfo.xml is the de-facto metadata standard for comic archives
//! (originating from ComicRack). We only extract the fields gideon needs
//! for display and library bookkeeping.

use quick_xml::events::Event;
use quick_xml::Reader;

/// Parsed subset of ComicInfo.xml.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ComicInfo {
    pub title: Option<String>,
    pub series: Option<String>,
    pub number: Option<String>,
    pub volume: Option<String>,
    pub writer: Option<String>,
    pub summary: Option<String>,
    pub page_count: Option<u32>,
}

impl ComicInfo {
    /// Parse ComicInfo XML. Unknown elements are ignored.
    pub fn parse(xml: &str) -> Result<Self, quick_xml::Error> {
        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(true);

        let mut info = ComicInfo::default();
        let mut current: Option<String> = None;

        loop {
            match reader.read_event()? {
                Event::Start(e) => {
                    current = Some(String::from_utf8_lossy(e.name().as_ref()).into_owned());
                }
                Event::End(_) => current = None,
                Event::Text(t) => {
                    if let Some(tag) = current.as_deref() {
                        let text = t.unescape()?.into_owned();
                        match tag {
                            "Title" => info.title = Some(text),
                            "Series" => info.series = Some(text),
                            "Number" => info.number = Some(text),
                            "Volume" => info.volume = Some(text),
                            "Writer" => info.writer = Some(text),
                            "Summary" => info.summary = Some(text),
                            "PageCount" => info.page_count = text.parse().ok(),
                            _ => {}
                        }
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }

        Ok(info)
    }

    /// Human-friendly title combining series and chapter title when both exist.
    pub fn display_title(&self) -> Option<String> {
        match (&self.series, &self.title) {
            (Some(series), Some(title)) if series != title => Some(format!("{series} — {title}")),
            (Some(series), _) => Some(series.clone()),
            (None, Some(title)) => Some(title.clone()),
            (None, None) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_fields() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
            <ComicInfo xmlns:xsd="http://www.w3.org/2001/XMLSchema">
              <Title>The Beginning</Title>
              <Series>Berserk</Series>
              <Number>1</Number>
              <Volume>1</Volume>
              <Writer>Kentaro Miura</Writer>
              <Summary>Guts &amp; the Band of the Hawk.</Summary>
              <PageCount>220</PageCount>
              <UnknownTag>ignored</UnknownTag>
            </ComicInfo>"#;

        let info = ComicInfo::parse(xml).unwrap();
        assert_eq!(info.title.as_deref(), Some("The Beginning"));
        assert_eq!(info.series.as_deref(), Some("Berserk"));
        assert_eq!(info.number.as_deref(), Some("1"));
        assert_eq!(info.volume.as_deref(), Some("1"));
        assert_eq!(info.writer.as_deref(), Some("Kentaro Miura"));
        assert_eq!(
            info.summary.as_deref(),
            Some("Guts & the Band of the Hawk.")
        );
        assert_eq!(info.page_count, Some(220));
    }

    #[test]
    fn empty_document_yields_defaults() {
        let info = ComicInfo::parse("<ComicInfo/>").unwrap();
        assert_eq!(info, ComicInfo::default());
        assert_eq!(info.display_title(), None);
    }

    #[test]
    fn display_title_combinations() {
        let both = ComicInfo {
            series: Some("Naruto".into()),
            title: Some("Enter Sasuke".into()),
            ..Default::default()
        };
        assert_eq!(
            both.display_title().as_deref(),
            Some("Naruto — Enter Sasuke")
        );

        let same = ComicInfo {
            series: Some("Naruto".into()),
            title: Some("Naruto".into()),
            ..Default::default()
        };
        assert_eq!(same.display_title().as_deref(), Some("Naruto"));

        let only_title = ComicInfo {
            title: Some("Oneshot".into()),
            ..Default::default()
        };
        assert_eq!(only_title.display_title().as_deref(), Some("Oneshot"));
    }
}
