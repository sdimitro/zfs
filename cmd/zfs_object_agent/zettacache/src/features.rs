use lazy_static::lazy_static;
use log::info;
use more_asserts::assert_lt;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fmt::Display};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum FeatureType {
    Upgradeable,
    NonUpgradeable,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Ord, PartialOrd, Hash)]
pub struct FeatureName(pub String);

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Feature {
    name: FeatureName,
    info: FeatureType,
}

lazy_static! {
    pub static ref SUPPORTED_FEATURES: HashMap<FeatureName, FeatureType> = [
        ORIGIN.clone(),
        EXPAND.clone(),
        GHOSTS.clone(),
        NOREMOVE.clone()
    ]
    .map(|feature| (feature.name, feature.info))
    .into_iter()
    .collect();
    pub static ref ORIGIN: Feature = Feature {
        name: FeatureName("com.delphix:origin".to_string()),
        info: FeatureType::Upgradeable
    };
    pub static ref EXPAND: Feature = Feature {
        name: FeatureName("com.delphix:expand".to_string()),
        info: FeatureType::NonUpgradeable
    };
    pub static ref GHOSTS: Feature = Feature {
        name: FeatureName("com.delphix:ghosts".to_string()),
        info: FeatureType::NonUpgradeable
    };
    pub static ref NOREMOVE: Feature = Feature {
        name: FeatureName("com.delphix:noremove".to_string()),
        info: FeatureType::NonUpgradeable
    };
}

#[derive(Debug)]
pub struct FeatureError {
    non_upgradeable_features: Vec<FeatureName>,
    unknown_features: Vec<FeatureName>,
}

impl Display for FeatureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        assert_lt!(
            0,
            self.non_upgradeable_features.len() + self.unknown_features.len(),
        );
        f.write_fmt(format_args!(
            "Missing Zettacache Features: {:?} - Unknown Features Encountered: {:?}",
            self.non_upgradeable_features, self.unknown_features,
        ))
    }
}

impl std::error::Error for FeatureError {}

pub fn check_features<'a, I>(feature_list: I) -> Result<(), FeatureError>
where
    I: IntoIterator<Item = &'a FeatureName>,
{
    let mut upgradeable_features = vec![];
    let mut non_upgradeable_features = vec![];
    let mut unknown_features = vec![];
    let mut supported_features = SUPPORTED_FEATURES.clone();

    for feature in feature_list {
        match supported_features.contains_key(feature) {
            true => {
                supported_features.remove(feature);
            }
            false => {
                unknown_features.push(feature.clone());
            }
        }
    }
    for (feature_name, feature_type) in supported_features {
        match feature_type {
            FeatureType::Upgradeable => upgradeable_features.push(feature_name),
            FeatureType::NonUpgradeable => non_upgradeable_features.push(feature_name),
        }
    }

    if !non_upgradeable_features.is_empty() || !unknown_features.is_empty() {
        Err(FeatureError {
            non_upgradeable_features,
            unknown_features,
        })
    } else {
        info!(
            "enabling the following upgradeable features: {:?}",
            upgradeable_features
        );
        Ok(())
    }
}
