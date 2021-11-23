// Functions which provide "nice" looking output for numbers

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
