use std::collections::BTreeMap;

/// Iterate over the values in the BTreeMap, starting with `start`, and looping
/// around such that all entries will be visited once.
pub fn iter_wrapping<K: Ord + Copy, V>(map: &BTreeMap<K, V>, start: K) -> impl Iterator<Item = &V> {
    map.range(start..)
        .chain(map.iter().take_while(move |(&k, _v)| k != start))
        .map(|(_k, v)| v)
}
