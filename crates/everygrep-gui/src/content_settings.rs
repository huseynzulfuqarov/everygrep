//! Settings of content search (`content:`): which files are read and how,
//! and the size of the hash cache of duplicate searches. They are on the
//! Search page.

use everygrep_core::content::{self, CodePage, ContentOptions};
use everygrep_core::hashcache;
use slint::{ModelRc, SharedString, VecModel};

use crate::i18n::tr;

/// Largest size limit the settings take, in MiB (1 TiB).
pub const MAX_SIZE_MB: u64 = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Larger files are not read, in MiB; 0 reads files of any size.
    pub max_size_mb: u64,
    /// The code page of files that are not UTF-8.
    pub code_page: CodePage,
    /// Read PDF files through pdftotext.
    pub pdf: bool,
    /// Largest size of the hash cache, in MiB; 0 keeps no hashes.
    pub hash_cache_mb: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_size_mb: content::DEFAULT_MAX_SIZE / (1024 * 1024),
            code_page: CodePage::default(),
            pdf: false,
            hash_cache_mb: hashcache::DEFAULT_MAX_BYTES / (1024 * 1024),
        }
    }
}

impl Settings {
    /// How the engine reads files.
    pub fn options(&self) -> ContentOptions {
        ContentOptions {
            max_size: self.max_size_mb * 1024 * 1024,
            code_page: self.code_page,
            pdf: self.pdf,
        }
    }

    /// The size limit of the hash cache in bytes.
    pub fn hash_cache_bytes(&self) -> u64 {
        self.hash_cache_mb * 1024 * 1024
    }

    pub fn fill_window(&self, w: &crate::SettingsWindow) {
        w.set_content_max_size(i32::try_from(self.max_size_mb).unwrap_or(i32::MAX));
        w.set_content_max_size_max(i32::try_from(MAX_SIZE_MB).unwrap_or(i32::MAX));
        w.set_content_code_page_names(ModelRc::new(VecModel::from(
            CodePage::ALL
                .into_iter()
                .map(|c| SharedString::from(code_page_name(c)))
                .collect::<Vec<_>>(),
        )));
        w.set_content_code_page(
            CodePage::ALL
                .iter()
                .position(|&c| c == self.code_page)
                .unwrap_or(0) as i32,
        );
        w.set_content_pdf(self.pdf);
        w.set_content_pdf_note(pdf_note(content::pdf_reader_available()).into());
        w.set_hash_cache_max(i32::try_from(self.hash_cache_mb).unwrap_or(i32::MAX));
        w.set_hash_cache_max_max(i32::try_from(MAX_SIZE_MB).unwrap_or(i32::MAX));
    }

    pub fn from_window(w: &crate::SettingsWindow) -> Self {
        Self {
            max_size_mb: u64::try_from(w.get_content_max_size())
                .unwrap_or(0)
                .min(MAX_SIZE_MB),
            code_page: usize::try_from(w.get_content_code_page())
                .ok()
                .and_then(|i| CodePage::ALL.get(i).copied())
                .unwrap_or_default(),
            pdf: w.get_content_pdf(),
            hash_cache_mb: u64::try_from(w.get_hash_cache_max())
                .unwrap_or(0)
                .min(MAX_SIZE_MB),
        }
    }

    pub fn set(&mut self, key: &str, value: &str) -> bool {
        match key {
            "content-max-size-mb" => value
                .parse::<u64>()
                .ok()
                .filter(|n| *n <= MAX_SIZE_MB)
                .map(|n| self.max_size_mb = n)
                .is_some(),
            "hash-cache-max-mb" => value
                .parse::<u64>()
                .ok()
                .filter(|n| *n <= MAX_SIZE_MB)
                .map(|n| self.hash_cache_mb = n)
                .is_some(),
            "content-code-page" => CodePage::from_id(value)
                .map(|c| self.code_page = c)
                .is_some(),
            "content-pdf" => match value {
                "true" | "yes" | "on" | "1" => {
                    self.pdf = true;
                    true
                }
                "false" | "no" | "off" | "0" => {
                    self.pdf = false;
                    true
                }
                _ => false,
            },
            _ => false,
        }
    }

    pub fn write(&self, out: &mut String) {
        let d = Self::default();
        if self.max_size_mb != d.max_size_mb {
            out.push_str(&format!("content-max-size-mb = {}\n", self.max_size_mb));
        }
        if self.code_page != d.code_page {
            out.push_str(&format!("content-code-page = {}\n", self.code_page.id()));
        }
        if self.pdf != d.pdf {
            out.push_str(&format!("content-pdf = {}\n", self.pdf));
        }
        if self.hash_cache_mb != d.hash_cache_mb {
            out.push_str(&format!("hash-cache-max-mb = {}\n", self.hash_cache_mb));
        }
    }
}

/// The name of a code page in the settings.
pub fn code_page_name(c: CodePage) -> &'static str {
    match c {
        CodePage::Windows1250 => tr("Windows-1250 (Central European)"),
        CodePage::Windows1251 => tr("Windows-1251 (Cyrillic)"),
        CodePage::Windows1252 => tr("Windows-1252 (Western European)"),
        CodePage::Windows1253 => tr("Windows-1253 (Greek)"),
        CodePage::Windows1254 => tr("Windows-1254 (Turkish, Azerbaijani)"),
        CodePage::Windows1257 => tr("Windows-1257 (Baltic)"),
    }
}

/// How much of the hash cache is used, under its size.
pub fn hash_cache_used(bytes: u64) -> String {
    crate::i18n::fill(tr("In use: {}"), &[&crate::format::size(bytes)])
}

/// The note under "Search inside PDF files": what it uses, or how to get it.
pub fn pdf_note(available: bool) -> &'static str {
    if available {
        tr(
            "PDF files are read with pdftotext in a separate process. Office documents are not searched yet.",
        )
    } else {
        tr(
            "pdftotext was not found. Install the poppler package (it provides pdftotext) to search inside PDF files.",
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::settings::{Page, Settings};
    use everygrep_core::content::CodePage;

    #[test]
    fn content_settings_roundtrip_reset_with_the_search_page_and_reject_bad_values() {
        let d = super::Settings::default();
        assert_eq!(
            (d.max_size_mb, d.code_page, d.pdf, d.hash_cache_mb),
            (100, CodePage::Windows1254, false, 512)
        );
        assert_eq!(d.hash_cache_bytes(), 512 * 1024 * 1024);
        assert_eq!(
            d.options(),
            everygrep_core::content::ContentOptions::default()
        );
        assert!(!Settings::default().to_text().contains("content-"));
        assert!(!Settings::default().to_text().contains("hash-cache"));
        let mut s = Settings::parse(
            "content-max-size-mb = 0\ncontent-code-page = windows-1251\ncontent-pdf = true\nhash-cache-max-mb = 0\n",
        );
        assert_eq!(s.content.hash_cache_bytes(), 0);
        assert_eq!(
            (s.content.max_size_mb, s.content.code_page, s.content.pdf),
            (0, CodePage::Windows1251, true)
        );
        assert_eq!(s.content.options().max_size, 0, "0 reads any size");
        assert_eq!(Settings::parse(&s.to_text()).content, s.content);
        s.reset(Page::Window);
        assert!(s.content.pdf);
        s.reset(Page::Search);
        assert_eq!(s.content, super::Settings::default());
        let bad = Settings::parse(
            "content-max-size-mb = -1\ncontent-max-size-mb = 99999999\ncontent-code-page = utf-8\ncontent-pdf = maybe\nhash-cache-max-mb = lots\n",
        );
        assert_eq!(bad.problems.len(), 5);
        assert_eq!(bad.content, super::Settings::default());
    }

    #[test]
    fn the_pdf_note_says_how_to_get_the_reader() {
        assert!(super::pdf_note(false).contains("poppler"));
        assert!(super::pdf_note(true).contains("separate process"));
        for page in CodePage::ALL {
            assert!(super::code_page_name(page).starts_with("Windows-"));
        }
    }
}
