use std::collections::HashMap;
use std::fmt::Display;

use lazy_static::lazy_static;
use log::info;
use more_asserts::assert_lt;
use serde::Deserialize;
use serde::Serialize;

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
        SLAB_INFO_SPACEMAP_ENTRIES.clone(),
        SLAB_ALLOCATOR.clone(),
        SLAB_SIZE_32MB.clone(),
        TRIMMED_INDEX.clone(),
        DISK_GUIDS.clone(),
        CACHE_DEVICE_REMOVAL.clone(),
    ]
    .map(|feature: Feature| (feature.name, feature.info))
    .into_iter()
    .collect();
    pub static ref SLAB_INFO_SPACEMAP_ENTRIES: Feature = Feature {
        name: FeatureName("com.delphix:slab_info_spacemap_entries".to_string()),
        info: FeatureType::NonUpgradeable
    };
    pub static ref SLAB_ALLOCATOR: Feature = Feature {
        name: FeatureName("com.delphix:slab_allocator".to_string()),
        info: FeatureType::NonUpgradeable
    };
    pub static ref SLAB_SIZE_32MB: Feature = Feature {
        name: FeatureName("com.delphix:slab_size_32mb".to_string()),
        info: FeatureType::Upgradeable
    };
    pub static ref TRIMMED_INDEX: Feature = Feature {
        name: FeatureName("com.delphix:trimmed_index".to_string()),
        info: FeatureType::Upgradeable
    };
    pub static ref DISK_GUIDS: Feature = Feature {
        name: FeatureName("com.delphix:disk_guids".to_string()),
        info: FeatureType::Upgradeable
    };
    pub static ref CACHE_DEVICE_REMOVAL: Feature = Feature {
        name: FeatureName("com.delphix:cache_device_removal".to_string()),
        info: FeatureType::Upgradeable
    };
}

#[derive(Debug)]
pub enum FeatureError {
    Unknown(FeatureName),
    NonUpgradeable(FeatureName),
}

#[derive(Debug)]
pub struct FeatureErrors {
    errors: Vec<FeatureError>,
}

impl Display for FeatureErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        assert_lt!(0, self.errors.len());
        f.write_fmt(format_args!(
            "Detected the following feature incompatibilities:\n"
        ))?;
        for feature in self.errors.iter() {
            f.write_fmt(format_args!("\t{:?}\n", feature))?;
        }
        Ok(())
    }
}

impl std::error::Error for FeatureErrors {}

pub fn check_features<'a, I>(feature_list: I) -> Result<(), FeatureErrors>
where
    I: IntoIterator<Item = &'a FeatureName>,
{
    let mut supported_features = SUPPORTED_FEATURES.clone();
    let mut upgradeable_features = vec![];
    let mut errors = vec![];

    for feature in feature_list {
        match supported_features.contains_key(feature) {
            true => {
                supported_features.remove(feature);
            }
            false => {
                errors.push(FeatureError::Unknown(feature.clone()));
            }
        }
    }
    for (feature_name, feature_type) in supported_features {
        match feature_type {
            FeatureType::Upgradeable => upgradeable_features.push(feature_name),
            FeatureType::NonUpgradeable => errors.push(FeatureError::NonUpgradeable(feature_name)),
        }
    }

    if !errors.is_empty() {
        Err(FeatureErrors { errors })
    } else {
        info!(
            "enabling the following upgradeable features: {:?}",
            upgradeable_features
        );
        Ok(())
    }
}
