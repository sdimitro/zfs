//! Functions which provide "nice" looking (human-readable) output for numbers.

use std::time::Duration;

/// Convert an arbitrary integer into a scaled byte length specification
/// with up to 4 significant digits (returned string will be <= 6 characters).
pub fn nice_p2size(number: u64) -> String {
    let mut scaled: f64 = number as f64;
    for unit in ["B", "KB", "MB", "GB", "TB", "PB", "EB", "ZB", "YB"] {
        let places = match scaled {
            x if x < 10.0 => 2,
            x if x < 100.0 => 1,
            x if x < 1024.0 => 0,
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
        let places = match scaled {
            x if x < 10.0 => 2,
            x if x < 100.0 => 1,
            x if x < 1000.0 => 0,
            _ => {
                scaled /= 1000.0;
                continue;
            }
        };
        if places > 0 {
            // Because format!() does some rounding of floating point numbers
            // (e.g: 9.999 with places == 2 will format to 10.00),
            // we need to take steps to enforce our character limit output.
            let scaled_string = &format!("{:.*}", places, scaled)[0..4];
            return format!("{}{}", scaled_string, unit);
        } else {
            return format!("{:.0}{}", scaled, unit);
        }
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
    let mut base = 1;
    for unit in ["ns", "us", "ms", "s"] {
        let mut places = match scaled {
            x if x < 10.0 => 2,
            x if x < 100.0 => 1,
            x if x < 1000.0 => 0,
            _ => {
                if unit != "s" {
                    scaled /= 1000.0;
                    base *= 1000;
                    continue;
                } else {
                    0
                }
            }
        };
        // If time is an even multiple of the base, then display without any decimal precision.
        if base > 1 && (nanoseconds % base) == 0 {
            places = 0;
        }
        return format!("{:.*}{}", places, scaled, unit);
    }
    panic!(
        "Coding error encountered; original number: {}, scaled value: {}",
        nanoseconds, scaled
    );
}
