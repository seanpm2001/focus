use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use std::str::FromStr;
use std::{collections::HashSet, convert::TryFrom, fmt::Display};

use thiserror::Error;

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct CoordinateSet {
    underlying: HashSet<Coordinate>,
    uniform: bool,
}

impl CoordinateSet {
    pub fn underlying(&self) -> &HashSet<Coordinate> {
        &self.underlying
    }

    pub fn is_uniform(&self) -> bool {
        self.uniform
    }

    pub fn determine_uniformity(set: &HashSet<Coordinate>) -> bool {
        let mut count_by_type = [0_usize; 3];

        for coordinate in set {
            match coordinate {
                Coordinate::Bazel(_) => count_by_type[0] += 1,
                Coordinate::Directory(_) => count_by_type[1] += 1,
                Coordinate::Pants(_) => count_by_type[2] += 1,
            }
        }

        let distinct_types_in_counts = count_by_type.into_iter().filter(|count| *count > 0).count();
        distinct_types_in_counts < 2
    }
}

impl From<HashSet<Coordinate>> for CoordinateSet {
    fn from(underlying: HashSet<Coordinate>) -> Self {
        let uniform = Self::determine_uniformity(&underlying);
        Self {
            underlying,
            uniform,
        }
    }
}

impl TryFrom<&[String]> for CoordinateSet {
    type Error = CoordinateError;

    fn try_from(coordinates: &[String]) -> Result<Self, Self::Error> {
        let mut underlying = HashSet::<Coordinate>::new();

        for coordinate in coordinates {
            match Coordinate::try_from(coordinate.as_str()) {
                Ok(coordinate) => {
                    underlying.insert(coordinate);
                }
                Err(e) => return Err(e),
            }
        }

        let uniform = Self::determine_uniformity(&underlying);
        Ok(Self {
            underlying,
            uniform,
        })
    }
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone)]
pub enum Coordinate {
    /// A Bazel package like `//foo/bar:baz`.
    Bazel(Label),

    /// A specific directory within the repository.
    Directory(String),

    /// A Pants package like `foo/bar:baz`.
    Pants(String),
}

impl Display for Coordinate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Coordinate::Bazel(c) => write!(f, "{}", c),
            Coordinate::Directory(c) => write!(f, "{}", c),
            Coordinate::Pants(c) => write!(f, "{}", c),
        }
    }
}

#[derive(Error, Debug, PartialEq)]
pub enum CoordinateError {
    #[error("Scheme not supported")]
    UnsupportedScheme(String),

    #[error("Failed to tokenize input")]
    TokenizationError,

    #[error("Failed to parse label")]
    LabelError(#[from] LabelParseError),
}

impl TryFrom<&str> for Coordinate {
    type Error = CoordinateError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value.split_once(':') {
            Some((prefix, rest)) => {
                let rest = rest.to_owned();
                if prefix.eq_ignore_ascii_case("bazel") {
                    let label: Label = rest.parse()?;
                    Ok(Coordinate::Bazel(label))
                } else if prefix.eq_ignore_ascii_case("directory") {
                    Ok(Coordinate::Directory(rest))
                } else if prefix.eq_ignore_ascii_case("pants") {
                    Ok(Coordinate::Pants(rest))
                } else {
                    Err(CoordinateError::UnsupportedScheme(prefix.to_owned()))
                }
            }
            None => Err(CoordinateError::TokenizationError),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum TargetName {
    Name(String),
    Ellipsis,
}

/// A Bazel label referring to a specific target.
///
/// See <https://docs.bazel.build/versions/main/build-ref.html#labels>. Note
/// that a label does *not* refer to a package.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Label {
    /// For a label like `@foo//bar:baz`, this would be `@foo`. If there is no
    /// `@`-component, then this is `None`.
    pub(crate) external_repository: Option<String>,

    /// The directory components of the path after `//`.
    ///
    /// The leading `//` is optional and inferred if present (i.e. a label
    /// `foo/bar` is assumed to be the same as `//foo/bar`, and not instead
    /// relative to the current directory.)
    pub(crate) path_components: Vec<String>,

    /// If no explicit target name is given, it is inferred from the last path
    /// component. For a label like `//foo/bar:bar` or `//foo/bar`, this would
    /// be `bar`.
    pub(crate) target_name: TargetName,
}

impl Display for Label {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "//{}",
            // Note that `path_components` may be empty, which is fine.
            self.path_components.join("/")
        )?;

        match &self.target_name {
            TargetName::Name(name) => {
                write!(f, ":{}", name)?;
            }
            TargetName::Ellipsis => {
                write!(f, "/...")?;
            }
        }

        Ok(())
    }
}

impl Debug for Label {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label_string: String = format!("{}", self);
        write!(f, r#"Label({:?})"#, label_string)
    }
}

/// TODO: improve error messaging here
#[derive(Error, Debug, PartialEq)]
pub enum LabelParseError {
    #[error("No target name")]
    NoTargetName,
}

impl FromStr for Label {
    type Err = LabelParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (external_package, label) = match s.split_once("//") {
            None => (None, s),
            Some(("", label)) => (None, label),
            Some((external_package, label)) => (Some(external_package.to_string()), label),
        };

        let mut path_components: Vec<String> = label.split('/').map(|s| s.to_string()).collect();
        let target_name = match path_components.pop() {
            Some(target_name) => target_name,
            None => return Err(LabelParseError::NoTargetName),
        };

        if target_name == "..." {
            Ok(Self {
                external_repository: external_package,
                path_components,
                target_name: TargetName::Ellipsis,
            })
        } else {
            let (last_component, target_name) = match target_name.split_once(':') {
                Some((last_component, target_name)) => (last_component, target_name),
                None => (target_name.as_str(), target_name.as_str()),
            };

            path_components.push(last_component.to_string());
            Ok(Self {
                external_repository: external_package,
                path_components,
                target_name: TargetName::Name(target_name.to_string()),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::convert::TryFrom;

    use anyhow::Result;

    use super::*;

    #[test]
    pub fn coordinate_parsing() -> Result<()> {
        assert_eq!(
            Coordinate::try_from("bazel://a:b")?,
            Coordinate::Bazel(Label {
                external_repository: None,
                path_components: vec!["a".to_string()],
                target_name: TargetName::Name("b".to_string()),
            })
        );
        assert_eq!(
            Coordinate::try_from("bazel://foo"),
            Ok(Coordinate::Bazel(Label {
                external_repository: None,
                path_components: vec!["foo".to_string()],
                target_name: TargetName::Name("foo".to_string())
            }))
        );
        assert_eq!(
            Coordinate::try_from("bazel://foo/bar/..."),
            Ok(Coordinate::Bazel(Label {
                external_repository: None,
                path_components: vec!["foo".to_string(), "bar".to_string()],
                target_name: TargetName::Ellipsis,
            }))
        );
        assert_eq!(
            Coordinate::try_from("bazel:@foo//bar:qux"),
            Ok(Coordinate::Bazel(Label {
                external_repository: Some("@foo".to_string()),
                path_components: vec!["bar".to_string()],
                target_name: TargetName::Name("qux".to_string()),
            }))
        );

        assert_eq!(
            Coordinate::try_from("bogus:whatever").unwrap_err(),
            CoordinateError::UnsupportedScheme("bogus".to_owned())
        );
        assert_eq!(
            Coordinate::try_from("okay").unwrap_err(),
            CoordinateError::TokenizationError
        );

        Ok(())
    }

    #[test]
    pub fn sets_from_strings_of_coordinates() -> Result<()> {
        let coordinates = vec![String::from("bazel://a:b"), String::from("bazel://x/y:z")];

        let set = CoordinateSet::try_from(coordinates.as_slice());
        let set = set.unwrap();
        assert_eq!(set.underlying().len(), 2);
        assert!(set.is_uniform());
        Ok(())
    }

    // TODO: Enable this again when there are more coordinate types.
    // #[cfg(disabled_test)]
    #[test]
    pub fn non_uniform_sets() -> Result<()> {
        // Sets containing different coordinate types are non-uniform
        assert!(!CoordinateSet::try_from(&[
            String::from("bazel://a:b"),
            String::from("directory:/foo"),
        ] as &[String])?
        .is_uniform());

        // Empty sets are uniform
        assert!(CoordinateSet::try_from(&[] as &[String])?.is_uniform());

        Ok(())
    }

    #[test]
    pub fn failed_conversion_of_sets() -> Result<()> {
        assert_eq!(
            CoordinateSet::try_from(&[String::from("whatever")] as &[String]).unwrap_err(),
            CoordinateError::TokenizationError
        );
        assert_eq!(
            CoordinateSet::try_from(&[String::from("foo:bar")] as &[String]).unwrap_err(),
            CoordinateError::UnsupportedScheme("foo".to_owned())
        );

        Ok(())
    }
}
