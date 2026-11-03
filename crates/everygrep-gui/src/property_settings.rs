//! Property cache limits and independent opt-in indexing rules. Only media
//! and document values are read from file contents, so only they can be
//! indexed ahead; the other properties come from the name index or one stat.
use crate::i18n::tr;
use everygrep_core::property::Property;
use everygrep_core::rules::{IndexRules, RuleSpec};
use slint::{ModelRc, SharedString, VecModel};

const MAX_MB: u64 = 1024 * 1024;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub enabled: bool,
    pub max_mb: u64,
    pub include: String,
    pub exclude: String,
}
impl Default for Rule {
    fn default() -> Self {
        Self {
            enabled: false,
            max_mb: 64,
            include: String::new(),
            exclude: String::new(),
        }
    }
}
/// The properties a rule exists for, in the order of [`Settings::rules`].
pub fn indexable() -> impl Iterator<Item = Property> {
    Property::ALL.into_iter().filter(|p| p.is_media())
}

impl Rule {
    pub fn filters(&self) -> Result<IndexRules, everygrep_core::rules::RuleError> {
        IndexRules::new(RuleSpec {
            include_files: self.include.clone(),
            exclude_files: self.exclude.clone(),
            ..RuleSpec::default()
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub cache_mb: u64,
    pub rules: Vec<Rule>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            cache_mb: 256,
            rules: vec![Rule::default(); indexable().count()],
        }
    }
}
impl Settings {
    pub fn set(&mut self, key: &str, value: &str) -> bool {
        if key == "property-cache-max-mb" {
            return value
                .parse::<u64>()
                .ok()
                .filter(|n| *n <= MAX_MB)
                .map(|n| self.cache_mb = n)
                .is_some();
        }
        for (i, p) in indexable().enumerate() {
            let Some(field) = key.strip_prefix(&format!("property-{}-", p.id())) else {
                continue;
            };
            let r = &mut self.rules[i];
            let flag = || match value {
                "true" | "yes" | "on" | "1" => Some(true),
                "false" | "no" | "off" | "0" => Some(false),
                _ => None,
            };
            return match field {
                "index" => flag().map(|v| r.enabled = v).is_some(),
                "max-size-mb" => value
                    .parse::<u64>()
                    .ok()
                    .filter(|n| *n <= MAX_MB)
                    .map(|n| r.max_mb = n)
                    .is_some(),
                "include" | "exclude" => crate::settings::unescape(value)
                    .and_then(|b| String::from_utf8(b).ok())
                    .map(|v| {
                        if field == "include" {
                            r.include = v
                        } else {
                            r.exclude = v
                        }
                    })
                    .is_some(),
                _ => false,
            };
        }
        false
    }
    pub fn write(&self, out: &mut String, all: bool) {
        let d = Self::default();
        if all || self.cache_mb != d.cache_mb {
            out.push_str(&format!("property-cache-max-mb = {}\n", self.cache_mb));
        }
        for (p, r) in indexable().zip(&self.rules) {
            let d = Rule::default();
            if all || r.enabled != d.enabled {
                out.push_str(&format!("property-{}-index = {}\n", p.id(), r.enabled));
            }
            if all || r.max_mb != d.max_mb {
                out.push_str(&format!("property-{}-max-size-mb = {}\n", p.id(), r.max_mb));
            }
            for (key, v) in [("include", &r.include), ("exclude", &r.exclude)] {
                if all || !v.is_empty() {
                    out.push_str(&format!(
                        "property-{}-{key} = {}\n",
                        p.id(),
                        crate::settings::escape(v.as_bytes())
                    ));
                }
            }
        }
    }
    pub fn fill(&self, w: &crate::SettingsWindow) {
        w.set_property_names(ModelRc::new(VecModel::from(
            indexable()
                .map(|p| SharedString::from(title(p)))
                .collect::<Vec<_>>(),
        )));
        w.set_property_cache_mb(self.cache_mb as i32);
        self.fill_rule(w);
    }
    pub fn fill_rule(&self, w: &crate::SettingsWindow) {
        let i = w.get_property_selected().max(0) as usize;
        let Some(r) = self.rules.get(i) else {
            return;
        };
        w.set_property_indexed(r.enabled);
        w.set_property_max_mb(r.max_mb as i32);
        w.set_property_include(r.include.as_str().into());
        w.set_property_exclude(r.exclude.as_str().into());
    }
    pub fn edited(&self, w: &crate::SettingsWindow) -> Result<Self, String> {
        let mut next = self.clone();
        next.cache_mb = w.get_property_cache_mb().max(0) as u64;
        if let Some(r) = next
            .rules
            .get_mut(w.get_property_selected().max(0) as usize)
        {
            *r = Rule {
                enabled: w.get_property_indexed(),
                max_mb: w.get_property_max_mb().max(0) as u64,
                include: w.get_property_include().into(),
                exclude: w.get_property_exclude().into(),
            };
            r.filters().map_err(|e| e.to_string())?;
        }
        Ok(next)
    }
}
/// The rule list's names; the column titles of the same properties.
pub fn title(p: Property) -> &'static str {
    use Property::*;
    match p {
        Width => tr("Width"),
        Height => tr("Height"),
        Dimensions => tr("Dimensions"),
        Length => tr("Length"),
        BitRate => tr("Bit rate"),
        Title => tr("Title"),
        Artist => tr("Artist"),
        Album => tr("Album"),
        Year => tr("Year"),
        Track => tr("Track"),
        Genre => tr("Genre"),
        Rating => tr("Rating"),
        Tags => tr("Tags"),
        PageCount => tr("Page count"),
        WordCount => tr("Word count"),
        CameraMaker => tr("Camera maker"),
        CameraModel => tr("Camera model"),
        DateTaken => tr("Date taken"),
        GpsLat => tr("GPS latitude"),
        GpsLong => tr("GPS longitude"),
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn individual_property_rules_round_trip_and_filter_names() {
        let mut settings = crate::settings::Settings::default();
        settings.properties.cache_mb = 12;
        let rule = &mut settings.properties.rules[0];
        rule.enabled = true;
        rule.max_mb = 2;
        rule.include = "*.png;*.jpg".into();
        rule.exclude = "private*".into();
        let filters = rule.filters().unwrap();
        assert!(filters.accepts_path(std::path::Path::new("/photos/a.png"), false));
        assert!(!filters.accepts_path(std::path::Path::new("/photos/private.png"), false));
        assert!(!filters.accepts_path(std::path::Path::new("/photos/a.wav"), false));
        assert_eq!(
            crate::settings::Settings::parse(&settings.to_text()).properties,
            settings.properties
        );
        let invalid = crate::settings::Settings::parse(
            "property-cache-max-mb = -1\nproperty-width-max-size-mb = invalid\n",
        );
        assert_eq!(invalid.problems.len(), 2);
        assert!(!Settings::default().rules.iter().any(|r| r.enabled));
        assert_eq!(indexable().next(), Some(Property::Width));
        // Rules exist only for values read from file contents.
        assert!(!Settings::default().set("property-full-path-index", "true"));
        assert!(Settings::default().set("property-width-index", "true"));
    }
}
