use std::borrow::Borrow;
use std::collections::BTreeMap;

/// Iterate over the (K,V) pairs in the BTreeMap, starting with `start`, and
/// looping around such that all entries will be visited once.
pub fn iter_wrapping<T: ?Sized + Ord + Copy, K: Borrow<T> + Ord + Copy, V>(
    map: &BTreeMap<K, V>,
    start: T,
) -> impl Iterator<Item = (&K, &V)> {
    map.range(start..)
        .chain(map.iter().take_while(move |(&k, _v)| k.borrow() != &start))
}
