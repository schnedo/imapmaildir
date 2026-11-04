use std::{collections::BTreeMap, path::PathBuf};

use derive_getters::Getters;
use serde::Deserialize;

use super::{Location, TagConfig, mapping::ProcessedTagError};

/// `[notmuch]` section of the account config
#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    config: Option<PathBuf>,
    profile: Option<String>,
    database: Option<PathBuf>,
    #[serde(default)]
    ignore_tags: Vec<String>,
    #[serde(default)]
    tags: BTreeMap<String, String>,
    processed_tag: Option<String>,
}

/// Syncing keywords of mails with notmuch tags
#[derive(Getters, Debug, Clone)]
pub struct Config {
    location: Location,
    tags: TagConfig,
}

impl TryFrom<ConfigFile> for Config {
    type Error = ProcessedTagError;

    /// # Errors
    /// If `processed_tag` can never be synced
    fn try_from(value: ConfigFile) -> Result<Self, Self::Error> {
        let config = Self {
            location: Location {
                config: value.config,
                profile: value.profile,
                database: value.database,
            },
            tags: TagConfig {
                // tags notmuch manages itself must never get synced
                ignored_tags: TagConfig::default()
                    .ignored_tags
                    .into_iter()
                    .chain(value.ignore_tags)
                    .collect(),
                tags: value.tags,
                processed_tag: value.processed_tag,
            },
        };
        config.tags.validate_processed_tag()?;

        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use assertables::*;

    use super::*;

    #[test]
    fn test_defaults_ignore_tags_managed_by_notmuch() {
        let config: ConfigFile = assert_ok!(toml::from_str(""));

        let notmuch = assert_ok!(Config::try_from(config));

        assert_eq!(notmuch.location(), &Location::default());
        assert_eq!(notmuch.tags(), &TagConfig::default());
    }

    #[test]
    fn test_rejects_unsyncable_processed_tag() {
        let config: ConfigFile = assert_ok!(toml::from_str(
            r#"
            ignore_tags = ["todo"]
            processed_tag = "todo"
            "#
        ));

        assert_err!(Config::try_from(config));
    }

    #[test]
    fn test_parses_all_settings() {
        let config: ConfigFile = assert_ok!(toml::from_str(
            r#"
            config = "/home/me/.notmuch-config"
            profile = "work"
            database = "/home/me/mail/.notmuch"
            ignore_tags = ["todo"]
            processed_tag = "processed"

            [tags]
            "$Junk" = "spam"
            "#
        ));

        let notmuch = assert_ok!(Config::try_from(config));

        assert_eq!(
            notmuch.location(),
            &Location {
                config: Some(PathBuf::from("/home/me/.notmuch-config")),
                profile: Some("work".to_string()),
                database: Some(PathBuf::from("/home/me/mail/.notmuch")),
            }
        );
        assert_contains!(notmuch.tags().ignored_tags, &"todo".to_string());
        assert_contains!(notmuch.tags().ignored_tags, &"unread".to_string());
        assert_eq!(notmuch.tags().processed_tag.as_deref(), Some("processed"));
        assert_eq!(
            notmuch.tags().tags,
            BTreeMap::from([("$Junk".to_string(), "spam".to_string())])
        );
    }
}
