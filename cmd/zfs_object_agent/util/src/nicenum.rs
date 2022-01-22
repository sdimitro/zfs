//! Functions which provide "nice" looking (human-readable) output for numbers.

use std::time::Duration;

/// Convert an arbitrary integer into a scaled byte length specification
/// with up to 4 significant digits (returned string will be <= 6 characters).
pub fn nice_p2size(number: u64) -> String {
    let mut scaled: f64 = number as f64;
    for unit in ["B", "KB", "MB", "GB", "TB", "PB", "EB", "ZB", "YB"] {
        // Note: !format() will "round" floating point data to the precision requested,
        // so the comparisons below take that into account when computing places
        let places = match scaled {
            x if x <= 9.995 => 2,
            x if x <= 99.95 => 1,
            x if x <= 1023.5 => 0,
            _ => {
                scaled /= 1024.0;
                continue;
            }
        };
        return format!("{:.*}{}", places, scaled, unit);
    }
    // it isn't possible to get here. 64 bits can only "count" to exabytes
    panic!(
        "Coding error encountered; original number: {}, scaled value: {}",
        number, scaled
    );
}

/// Format a counter value with up to 4 significant digits and the appropriate
/// unit scale.  The returned string will fit in 5 characters.
pub fn nice_number_count(number: f64) -> String {
    let mut scaled = number;
    for unit in ["", "K", "M", "G", "T", "P", "E", "Z", "Y"] {
        // Note: format!() will "round" floating point data to the precision requested,
        // so the comparisons below take that into account when computing places
        let places = match scaled {
            x if x <= 9.995 => 2,
            x if x <= 99.95 => 1,
            x if x <= 999.5 => 0,
            _ => {
                scaled /= 1000.0;
                continue;
            }
        };
        return format!("{:.*}{}", places, scaled, unit);
    }
    // it isn't possible to get here.
    panic!("Coding error encountered; original number: {}", number);
}

/// Format a time **duration** with up to 4 significant digits and the appropriate
/// time unit between 'ns' up to 's' (seconds).  The returned string will fit in
/// 5 characters unless the represented time is > 9,999 seconds.
pub fn nice_number_time(time: Duration) -> String {
    let nanoseconds: u64 = time.as_nanos().try_into().unwrap();
    if nanoseconds == 0 {
        // Don't print zero latencies since they're invalid
        return String::from("-");
    }
    let mut scaled: f64 = nanoseconds as f64;
    for unit in ["ns", "us", "ms", "s"] {
        let places = match scaled {
            // Note: format!() will "round" floating point data to the precision requested,
            // so the comparisons below take that into account when computing places
            x if x <= 9.995 => 2,
            x if x <= 99.95 => 1,
            x if x <= 999.5 => 0,
            _ => {
                if unit != "s" {
                    scaled /= 1000.0;
                    continue;
                } else {
                    0
                }
            }
        };
        return format!("{:.*}{}", places, scaled, unit);
    }
    panic!(
        "Coding error encountered; original number: {}, scaled value: {}",
        nanoseconds, scaled
    );
}

/// Verify that rounding is not breaking our character limit contract.
/// For example, 102,349 should not come backs as "100.0KB".
#[test]
fn test_char_limit_for_nice_number() {
    for x in [
        (1024 * 10) - 1,
        (1024 * 100) - 1,
        (1024 * 1024 * 10) - 1,
        (1024 * 1024 * 100) - 1,
        (1024 * 1024 * 1024 * 10) - 1,
        (1024 * 1024 * 1024 * 100) - 1,
        (1024 * 1024 * 1024 * 1024 * 10) - 1,
        (1024 * 1024 * 1024 * 1024 * 100) - 1,
    ] {
        let nice = nice_p2size(x);
        if nice.len() > 6 {
            panic!("{} exceeds 6 chars", nice);
        }
        let nice = nice_number_count(x as f64);
        if nice.len() > 5 {
            panic!("{} exceeds 5 chars", nice);
        }
    }
}

/// Verify that rounding is not breaking our character limit contract.
/// For example, 99,999 should not come backs as "100.0us".
#[test]
fn test_char_limit_for_nice_time() {
    for n in [9999, 99999, 9999999, 99999999] {
        let nice = nice_number_time(Duration::from_nanos(n));
        if nice.len() > 6 {
            panic!("{} exceeds 6 chars", nice);
        }
    }
}
