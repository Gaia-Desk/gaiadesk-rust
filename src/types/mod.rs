//! The API's shapes: requests the SDK sends (specs, with builders) and the
//! results it reads, field for field as `api/openapi.yaml` and GaiaDesk's
//! JSON Schema (`client/schema.json`) define them. Field names are the wire's
//! own (snake_case), so these read the same as the API's docs.
//!
//! Results are read leniently: a missing field takes its default, and fields
//! this SDK does not know yet are kept in `extra`.

/// A string enum with a catch-all for values this SDK does not know.
macro_rules! string_enum {
    ($(#[$m:meta])* $name:ident { $($(#[$vm:meta])* $v:ident = $s:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        pub enum $name {
            $($(#[$vm])* $v,)+
            /// A value this SDK does not know yet.
            Other(String),
        }

        impl $name {
            /// Its wire name.
            pub fn as_str(&self) -> &str {
                match self {
                    $($name::$v => $s,)+
                    $name::Other(s) => s,
                }
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                match s {
                    $($s => $name::$v,)+
                    other => $name::Other(other.to_string()),
                }
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                $name::from(s.as_str())
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                Ok($name::from(String::deserialize(d)?))
            }
        }
    };
}

mod fleet;
mod ops;

pub use fleet::*;
pub use ops::*;
