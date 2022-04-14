use std::any::type_name;
use std::fmt::Debug;
use std::sync::atomic::*;
use std::sync::RwLock;
use std::time::Duration;

use anyhow::anyhow;
use anyhow::Result;
pub use anyhow::Result as TunableResult;
use bytesize::ByteSize;
use config::Config;
use config::ConfigError;
use config::Value;
use config::ValueKind;
pub use lazy_static::lazy_static;
use log::*;
use num_traits::AsPrimitive;
use num_traits::Num;
use num_traits::NumCast;
use serde::de::DeserializeOwned;

lazy_static! {
    pub static ref CONFIG: RwLock<Config> = Default::default();
}

/// The `tunable!` macro is similar to `lazy_static!` (which it uses internally), but is used for
/// declaring configuration parameters.  The primary advantage is not having to match the
/// variable name (SCREAMING_SNAKE_CASE) with the string that's used in the config file
/// (snake_case).  The syntax is the same as `lazy_static!`, e.g.:
/// ```
/// use util::tunable;
/// tunable! {
///     static ref VARIABLE_NAME: u64 = 123;
/// }
/// ```
/// However, at runtime the variable's value may be overridden by by the config file that was
/// loaded with `read_config()`.
///
/// The type of the variable must implement the `Tunable` trait, which is used to convert from
/// the value in the config file (e.g. a String) to the target type (e.g. a Duration).
/// Implementations of `Tunable` are provided for the base integer types (`u64`, etc),
/// `std::time::Duration`, `chrono::Duration`, `ByteSize`, and the included `ByteSize32` and
/// `Percent` types.
#[macro_export]
macro_rules! tunable {
    // "pub" variant
    (pub static ref $N:ident : $T:ty = $e:expr; $($t:tt)*) => {
        $crate::tunable::lazy_static! {
            pub static ref $N: $T =
                $crate::tunable::get_tunable(&stringify!($N).to_lowercase(), $e);
        }
        tunable!($($t)*);
    };
    // non-"pub" variant
    (static ref $N:ident : $T:ty = $e:expr; $($t:tt)*) => {
        $crate::tunable::lazy_static! {
            static ref $N: $T = $crate::tunable::get_tunable(&stringify!($N).to_lowercase(), $e);
        }
        tunable!($($t)*);
    };

    () => ()
}

pub fn get_tunable<T>(name: &str, default: T) -> T
where
    T: Tunable,
{
    match CONFIG.read().unwrap().get::<T::Input>(name) {
        Ok(raw) => {
            let raw_string = format!("{:?}", raw);
            match T::convert(raw) {
                Ok(baked) => {
                    info!(
                        "{}: using value {:?} from config (converted from {})",
                        name, baked, raw_string
                    );
                    baked
                }
                Err(e) => {
                    warn!(
                        "{}: error converting tunable from {} ({}) to {}: {}; using default: {:?}",
                        name,
                        raw_string,
                        type_name::<T::Input>(),
                        type_name::<T>(),
                        e,
                        default
                    );
                    default
                }
            }
        }
        Err(ConfigError::NotFound(_)) => default,
        Err(e) => {
            warn!(
                "{}: error getting tunable as {}: {}; using default: {:?}",
                name,
                type_name::<T>(),
                e,
                default
            );
            default
        }
    }
}

/// A configuration tunable that's based on a DeserializeOwned type.
pub trait Tunable: Debug {
    type Input: DeserializeOwned + Debug;
    fn convert(input: Self::Input) -> Result<Self>
    where
        Self: Sized;
}

/// Implement the `Tunable` trait of the given type with a no-op conversion.  The type must be
/// `DeserializeOwned + Debug`.
#[macro_export]
macro_rules! tunable_convert_noop {
    ($t:ty) => {
        impl $crate::tunable::Tunable for $t {
            type Input = Self;
            fn convert(input: Self::Input) -> $crate::tunable::TunableResult<Self> {
                Ok(input)
            }
        }
    };
}

tunable_convert_noop!(bool);
tunable_convert_noop!(u8);
tunable_convert_noop!(u16);
tunable_convert_noop!(u32);
tunable_convert_noop!(u64);
tunable_convert_noop!(usize);
tunable_convert_noop!(i8);
tunable_convert_noop!(i16);
tunable_convert_noop!(i32);
tunable_convert_noop!(i64);
tunable_convert_noop!(isize);
tunable_convert_noop!(f32);
tunable_convert_noop!(f64);
tunable_convert_noop!(AtomicBool);
tunable_convert_noop!(AtomicU8);
tunable_convert_noop!(AtomicU16);
tunable_convert_noop!(AtomicU32);
tunable_convert_noop!(AtomicU64);
tunable_convert_noop!(AtomicUsize);
tunable_convert_noop!(AtomicI8);
tunable_convert_noop!(AtomicI16);
tunable_convert_noop!(AtomicI32);
tunable_convert_noop!(AtomicI64);
tunable_convert_noop!(AtomicIsize);
tunable_convert_noop!(String);

impl<T> Tunable for Option<T>
where
    T: Tunable,
{
    type Input = T::Input;
    fn convert(input: Self::Input) -> Result<Self> {
        match T::convert(input) {
            Ok(v) => Ok(Some(v)),
            Err(e) => Err(e),
        }
    }
}

/// A configuration tunable that's based on another `Tunable` type, rather than a
/// `DeserializeOwned` type.  This can be used to provide additional constraints for a base
/// Tunable.  For example, a u64 that can't be more than 72.  If a larger value is specified in
/// the config file, the `convert()` method can either saturate (return `Ok(72)`) or fail (return
/// `Err`).
pub trait LayeredTunable: Debug {
    type Input: Tunable;
    fn convert(input: Self::Input) -> Result<Self>
    where
        Self: Sized;
}

impl<T> Tunable for T
where
    T: LayeredTunable,
{
    type Input = <<T as LayeredTunable>::Input as Tunable>::Input;

    fn convert(input: Self::Input) -> TunableResult<Self> {
        <T as LayeredTunable>::convert(<<T as LayeredTunable>::Input as Tunable>::convert(input)?)
    }
}

impl Tunable for Duration {
    type Input = String;
    fn convert(input: Self::Input) -> Result<Self> {
        Ok(humantime::parse_duration(&input)?)
    }
}

impl Tunable for chrono::Duration {
    type Input = String;
    fn convert(input: Self::Input) -> Result<Self> {
        Ok(chrono::Duration::from_std(humantime::parse_duration(
            &input,
        )?)?)
    }
}

impl Tunable for ByteSize {
    type Input = String;
    fn convert(input: Self::Input) -> Result<Self> {
        input.parse::<ByteSize>().map_err(|str| anyhow!(str))
    }
}

/// Like ByteSize, but based on a u32 rather than a u64.  If the setting in the config file is
/// too large to be converted to a u32, conversion will fail and the default value will be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ByteSize32(u32);
impl LayeredTunable for ByteSize32 {
    type Input = ByteSize;
    fn convert(input: Self::Input) -> TunableResult<Self> {
        Ok(ByteSize32(input.as_u64().try_into()?))
    }
}
impl ByteSize32 {
    pub fn b(size: u32) -> Self {
        Self(size)
    }
    pub fn kib(size: u32) -> Self {
        Self(size * 1024)
    }
    pub fn mib(size: u32) -> Self {
        Self(size * 1024 * 1024)
    }
    pub fn as_u32(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Percent(f64);
impl Tunable for Percent {
    type Input = Value;
    fn convert(input: Self::Input) -> Result<Self> {
        if let ValueKind::String(s) = input.kind {
            Ok(Percent::new(s.strip_suffix('%').unwrap_or(&s).parse()?))
        } else {
            Ok(Percent::new(input.into_float()?))
        }
    }
}
impl Percent {
    pub fn new(percent: f64) -> Self {
        Self(percent)
    }

    /// Return the fraction of a whole that this percent represents.
    ///
    /// ## Example
    /// ```
    /// use util::tunable::Percent;
    /// assert_eq!(Percent::new(25.0).as_fraction(), 0.25);
    /// ```
    pub fn as_fraction(self) -> f64 {
        self.0 / 100.0
    }

    /// Return the percent.
    ///
    /// ## Example
    /// ```
    /// use util::tunable::Percent;
    /// assert_eq!(Percent::new(25.0).as_percent(), 25.0);
    /// ```
    pub fn as_percent(self) -> f64 {
        self.0
    }

    /// Apply (i.e. multiply) this percent to another number.
    ///
    /// ## Example
    /// ```
    /// use util::tunable::Percent;
    /// assert_eq!(Percent::new(25.0).apply(10.0f64), 2.5);
    /// assert_eq!(Percent::new(25.0).apply(10), 2);
    /// ```
    pub fn apply<N: Num + NumCast + AsPrimitive<f64> + Copy + Debug>(self, rhs: N) -> N {
        NumCast::from(self.as_fraction() * (rhs.as_())).unwrap()
    }
}

pub fn read_config(file_name: &str) -> Result<()> {
    let mut config = CONFIG.write().unwrap();
    *config = Config::builder()
        .add_source(config::File::with_name(file_name))
        .build()?;
    Ok(())
}

pub fn log_config() {
    let config = CONFIG.read().unwrap();
    info!("config: {}", config.cache);
}

#[cfg(test)]
mod test_tunable {
    use config::FileFormat;
    use serial_test::serial;

    use super::*;

    fn config(s: &str) {
        let mut config = CONFIG.write().unwrap();
        *config = Config::builder()
            .add_source(config::File::from_str(s, FileFormat::Toml))
            .build()
            .unwrap();
    }

    macro_rules! test {
        ($name: ident, $config: expr, $type: ty, $expected: expr) => {
            #[test]
            #[serial]
            fn $name() {
                tunable! { static ref TEST: $type = Default::default(); }
                config($config);
                assert_eq!(*TEST, $expected);
            }
        };
    }

    macro_rules! def {
        ($name: ident, $config: expr, $type: ty) => {
            test!($name, $config, $type, <$type as Default>::default());
        };
    }

    def!(default_u32, "", u32);
    def!(comment, "#test = 123", u32);
    test!(set32, "test = 123", u32, 123);
    test!(set32_str, "test = \"123\"", u32, 123);
    def!(default_string, "", String);
    test!(setstr, "test = \"bar\"", String, "bar");

    def!(option_none, "", Option<usize>);
    test!(option_some, "test = 123", Option<usize>, Some(123));

    test!(
        duration_ms,
        "test = \"123 ms\"",
        Duration,
        Duration::from_millis(123)
    );
    test!(
        duration_seconds,
        "test = \"123 seconds\"",
        Duration,
        Duration::from_secs(123)
    );
    test!(
        duration_s,
        "test = \"123s\"",
        Duration,
        Duration::from_secs(123)
    );
    def!(duration_unitless, "test = \"123\"", Duration);
    def!(duration_number, "test = 123", Duration);
    test!(bytesize_number, "test = 123", ByteSize, ByteSize::b(123));
    test!(
        bytesize_unitless,
        "test = \"123\"",
        ByteSize,
        ByteSize::b(123)
    );
    test!(bytesize_b, "test = \"123b\"", ByteSize, ByteSize::b(123));
    test!(bytesize_k, "test = \"123K\"", ByteSize, ByteSize::b(123000));
    test!(
        bytesize_kib,
        "test = \"123 KiB\"",
        ByteSize,
        ByteSize::kib(123)
    );
    test!(
        bytesize_pib,
        "test = \"123 PiB\"",
        ByteSize,
        ByteSize::pib(123)
    );

    test!(
        bytesize32_k,
        "test = \"123K\"",
        ByteSize32,
        ByteSize32::b(123000)
    );
    def!(bytesize32_pib, "test = \"123 PiB\"", ByteSize32);

    test!(percent_number, "test = 123", Percent, Percent::new(123.0));
    test!(percent_decimal, "test = 1.25", Percent, Percent::new(1.25));
    test!(
        percent_string,
        "test = \"123\"",
        Percent,
        Percent::new(123.0)
    );
    test!(
        percent_string_pct,
        "test = \"123%\"",
        Percent,
        Percent::new(123.0)
    );
    test!(
        percent_string_pct_decimal,
        "test = \"1.25%\"",
        Percent,
        Percent::new(1.25)
    );
}
