//! This module provides a mechanism to have the Agent process randmly exit at
//! certain "interesting" points, to test recovery on restart.  To use it, set
//! the "die_mtbf_secs" tunable to the desired mean time between failures, in
//! seconds.  A time point between 0 and 2x the configured time will be selected
//! as the amount of time to run before dying.  At that point, a random call
//! site of `maybe_die_with()` will be selected to exit the process.
//!
//! Note that each *call site* (source file, line, column) is equally likely to
//! die, not each *call* (invocation of maybe_die_with()).  For example,
//! maybe_die_with() is called 1000x/sec from one call site and 1x/sec from
//! another call site, we will be equally likely to terminate via each of the 2
//! call sites.  Therefore you don't need to worry about adding a high-frequency
//! caller and having it "always" die on that caller.

use crate::get_tunable;
use lazy_static::lazy_static;
use log::*;
use std::{
    collections::HashSet,
    fmt::Display,
    panic::Location,
    sync::RwLock,
    time::{Duration, Instant},
};

lazy_static! {
    // RUN_TIME is a random amount between 0 and 2x the configured MTBF (Mean
    // Time Between Failures)
    // XXX use humantime::parse_duration so it can be hours, etc?
    static ref RUN_TIME: Option<Duration> = get_tunable("die_mtbf_secs", None)
        .map(|secs: f64| Duration::from_secs_f64(secs * rand::random::<f64>() * 2.0));
    static ref LOCATIONS: RwLock<HashSet<&'static Location<'static>>> = Default::default();
    static ref BEGIN: Instant = Instant::now();
}

// Instead of taking a string (or Display) to print, this takes a function which
// returns the Display.  This is so that the caller don't have the cost of
// generating the string on every call, only when we're actually dying.
// track_caller ensures that Location::caller() will capture the call site of this function
#[track_caller]
pub fn maybe_die_with<M, F>(f: F)
where
    F: FnOnce() -> M,
    M: Display,
{
    if let Some(run_time) = *RUN_TIME {
        let location = Location::caller();
        if !LOCATIONS.read().unwrap().contains(location) {
            LOCATIONS.write().unwrap().insert(location);
        }
        if BEGIN.elapsed() >= run_time {
            // Check if this is the location chosen to die.  We declare
            // DIE_LOCATION here so that we're sure to not evaluate it until
            // RUN_TIME has elapsed, so that LOCATIONS has been filled in.
            lazy_static! {
                static ref DIE_LOCATION: &'static Location<'static> = {
                    let locations = LOCATIONS.read().unwrap();
                    let die_location = *locations
                        .iter()
                        .nth(rand::random::<usize>() % locations.len())
                        .unwrap();
                    warn!(
                        "after running {} seconds, selected site to die: {}",
                        RUN_TIME.unwrap().as_secs(),
                        die_location
                    );
                    die_location
                };
            }
            if location == *DIE_LOCATION {
                let msg = f();
                warn!("exiting to test failure handling: {}", msg);
                panic!("exiting to test failure handling: {}", msg);
            }
        }
    }
}
