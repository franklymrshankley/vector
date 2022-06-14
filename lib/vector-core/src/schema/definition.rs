use std::collections::{BTreeMap, BTreeSet};

use crate::config::LogNamespace;
use lookup::LookupBuf;
use value::kind::insert;
use value::{
    kind::{merge, Collection},
    Kind,
};

/// The definition of a schema.
///
/// This struct contains all the information needed to inspect the schema of an event emitted by
/// a source/transform.
#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct Definition {
    /// The type of the event
    kind: Kind,

    /// Semantic meaning assigned to fields within the collection.
    ///
    /// The value within this map points to a path inside the `collection`. It is an invalid state
    /// for there to be a meaning pointing to a non-existing path in the collection.
    meaning: BTreeMap<String, MeaningPointer>,

    /// Type definitions of components can change depending on the log namespace chosen.
    /// This records which ones are possible.
    /// An empty set means the definition can't be for a log
    log_namespaces: BTreeSet<LogNamespace>,
}

/// In regular use, a semantic meaning points to exactly _one_ location in the collection. However,
/// when merging two [`Definition`]s, we need to be able to allow for two definitions with the same
/// semantic meaning identifier to be merged together.
///
/// We cannot error when this happens, because a follow-up component (such as the `remap`
/// transform) might rectify the issue of having a semantic meaning with multiple pointers.
///
/// Because of this, we encapsulate this state in an enum. The schema validation step done by the
/// sink builder, will return an error if the definition stores an "invalid" meaning pointer.
#[derive(Clone, Debug, PartialEq, PartialOrd)]
enum MeaningPointer {
    Valid(LookupBuf),
    Invalid(BTreeSet<LookupBuf>),
}

impl MeaningPointer {
    fn merge(self, other: Self) -> Self {
        let set = match (self, other) {
            (Self::Valid(lhs), Self::Valid(rhs)) if lhs == rhs => return Self::Valid(lhs),
            (Self::Valid(lhs), Self::Valid(rhs)) => BTreeSet::from([lhs, rhs]),
            (Self::Valid(lhs), Self::Invalid(mut rhs)) => {
                rhs.insert(lhs);
                rhs
            }
            (Self::Invalid(mut lhs), Self::Valid(rhs)) => {
                lhs.insert(rhs);
                lhs
            }
            (Self::Invalid(mut lhs), Self::Invalid(rhs)) => {
                lhs.extend(rhs);
                lhs
            }
        };

        Self::Invalid(set)
    }
}

#[cfg(test)]
impl From<&str> for MeaningPointer {
    fn from(v: &str) -> Self {
        MeaningPointer::Valid(v.into())
    }
}

#[cfg(test)]
impl From<LookupBuf> for MeaningPointer {
    fn from(v: LookupBuf) -> Self {
        MeaningPointer::Valid(v)
    }
}

impl Definition {
    /// Create an "empty" definition.
    ///
    /// This means no type information is known about the event.
    #[deprecated]
    pub fn empty() -> Self {
        Self {
            kind: Kind::object(Collection::empty()),
            meaning: BTreeMap::default(),
            // this is incorrect, but the func is being deleted anyway...
            log_namespaces: BTreeSet::new(),
        }
    }

    pub fn any() -> Self {
        Self::empty_kind(Kind::any(), [LogNamespace::Legacy, LogNamespace::Vector])
    }

    /// Creates an empty definition that is of the kind specified.
    /// There are no meanings or optional fields.
    /// The log_namespaces are used to list the possible namespaces the schema is for.
    pub fn empty_kind(kind: Kind, log_namespaces: impl Into<BTreeSet<LogNamespace>>) -> Self {
        Self {
            kind,
            meaning: BTreeMap::default(),
            log_namespaces: log_namespaces.into(),
        }
    }

    /// An object with any fields, and the `Legacy` namespace.
    /// This is what most sources use for the legacy namespace.
    pub fn legacy_default() -> Self {
        Self::empty_kind(Kind::any_object(), [LogNamespace::Legacy])
    }

    /// Returns the default source schema for a source that produce the listed log namespaces
    pub fn default_for_namespace(log_namespaces: &BTreeSet<LogNamespace>) -> Self {
        let is_legacy = log_namespaces.contains(&LogNamespace::Legacy);
        let is_vector = log_namespaces.contains(&LogNamespace::Vector);
        match (is_legacy, is_vector) {
            (false, false) => Self::empty_kind(Kind::any(), []),
            (true, false) => Self::legacy_default(),
            (false, true) => Self::empty_kind(Kind::any(), [LogNamespace::Vector]),
            (true, true) => {
                Self::empty_kind(Kind::any(), [LogNamespace::Legacy, LogNamespace::Vector])
            }
        }
    }

    pub fn log_namespaces(&self) -> &BTreeSet<LogNamespace> {
        &self.log_namespaces
    }

    /// Add type information for an event field.
    /// A non-root required field means the root type must be an object, so the type will be automatically
    /// restricted to an object.
    ///
    /// # Panics
    /// - If the path is not root, and the definition does not allow the type to be an object
    /// - Provided path has one or more coalesced segments (e.g. `.(foo | bar)`).
    #[must_use]
    pub fn with_field(
        mut self,
        path: impl Into<LookupBuf>,
        kind: Kind,
        meaning: Option<&str>,
    ) -> Self {
        let path = path.into();
        let meaning = meaning.map(ToOwned::to_owned);

        if !path.is_root() {
            self.kind = self
                .kind
                .into_object()
                .expect("required field implies the type can be an object")
                .into();
        }

        if let Err(err) = self.kind.insert_at_path(
            &path.to_lookup(),
            kind,
            insert::Strategy {
                inner_conflict: insert::InnerConflict::Replace,
                leaf_conflict: insert::LeafConflict::Replace,
                coalesced_path: insert::CoalescedPath::Reject,
            },
        ) {
            panic!("Field definition not valid: {:?}", err);
        }

        if let Some(meaning) = meaning {
            self.meaning.insert(meaning, MeaningPointer::Valid(path));
        }

        self
    }

    /// Add type information for an optional event field.
    ///
    /// # Panics
    ///
    /// See `Definition::require_field`.
    #[must_use]
    pub fn optional_field(
        self,
        path: impl Into<LookupBuf>,
        kind: Kind,
        meaning: Option<&str>,
    ) -> Self {
        self.with_field(path, kind.or_null(), meaning)
    }

    /// Register a semantic meaning for the definition.
    ///
    /// # Panics
    ///
    /// This method panics if the provided path points to an unknown location in the collection.
    pub fn with_known_meaning(mut self, path: impl Into<LookupBuf>, meaning: &str) -> Self {
        let path = path.into();

        // Ensure the path exists in the collection.
        assert!(self
            .kind
            .find_known_at_path(&mut path.to_lookup())
            .ok()
            .flatten()
            .is_some());

        self.meaning
            .insert(meaning.to_owned(), MeaningPointer::Valid(path));
        self
    }

    /// Set the kind for all unknown fields.
    #[must_use]
    pub fn unknown_fields(mut self, unknown: impl Into<Option<Kind>>) -> Self {
        let unknown = unknown.into();
        if let Some(object) = self.kind.as_object_mut() {
            object.set_unknown(unknown.clone());
        }
        if let Some(array) = self.kind.as_array_mut() {
            array.set_unknown(unknown);
        }
        self
    }

    /// Merge `other` definition into `self`.
    ///
    /// The merge strategy for optional fields is as follows:
    ///
    /// If the field is marked as optional in both definitions, _or_ if it's optional in one,
    /// and unspecified in the other, then the field remains optional.
    ///
    /// If it's marked as "required" in either of the two definitions, then it becomes
    /// a required field in the merged definition.
    ///
    /// Note that it is allowed to have required field nested under optional fields. For
    /// example, `.foo` might be set as optional, but `.foo.bar` as required. In this case, it
    /// means that the object at `.foo` is allowed to be missing, but if it's present, then it's
    /// required to have a `bar` field.
    #[must_use]
    pub fn merge(mut self, other: Self) -> Self {
        for (other_id, other_meaning) in other.meaning {
            let meaning = match self.meaning.remove(&other_id) {
                Some(this_meaning) => this_meaning.merge(other_meaning),
                None => other_meaning,
            };

            self.meaning.insert(other_id, meaning);
        }

        self.kind.merge(
            other.kind,
            merge::Strategy {
                depth: merge::Depth::Deep,
                indices: merge::Indices::Keep,
            },
        );

        self
    }

    /// Returns a `Lookup` into an event, based on the provided `meaning`, if the meaning exists.
    pub fn meaning_path(&self, meaning: &str) -> Option<&LookupBuf> {
        match self.meaning.get(meaning) {
            Some(MeaningPointer::Valid(path)) => Some(path),
            None | Some(MeaningPointer::Invalid(_)) => None,
        }
    }

    pub fn invalid_meaning(&self, meaning: &str) -> Option<&BTreeSet<LookupBuf>> {
        match &self.meaning.get(meaning) {
            Some(MeaningPointer::Invalid(paths)) => Some(paths),
            None | Some(MeaningPointer::Valid(_)) => None,
        }
    }

    pub fn meanings(&self) -> impl Iterator<Item = (&String, &LookupBuf)> {
        self.meaning
            .iter()
            .filter_map(|(id, pointer)| match pointer {
                MeaningPointer::Valid(path) => Some((id, path)),
                MeaningPointer::Invalid(_) => None,
            })
    }

    pub fn kind(&self) -> &Kind {
        &self.kind
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use super::*;

    #[test]
    fn test_required_field() {
        struct TestCase {
            path: LookupBuf,
            kind: Kind,
            meaning: Option<&'static str>,
            want: Definition,
        }

        for (
            title,
            TestCase {
                path,
                kind,
                meaning,
                want,
            },
        ) in HashMap::from([
            (
                "simple",
                TestCase {
                    path: "foo".into(),
                    kind: Kind::boolean(),
                    meaning: Some("foo_meaning"),
                    want: Definition::empty_kind(
                        Kind::object(BTreeMap::from([("foo".into(), Kind::boolean())])),
                        [],
                    )
                    .with_known_meaning("foo", "foo_meaning"),
                    // want: Definition {
                    //     kind: Kind::object(BTreeMap::from([("foo".into(), Kind::boolean())])),
                    // collection: BTreeMap::from([("foo".into(), Kind::boolean())]).into(),
                    // meaning: [("foo_meaning".to_owned(), "foo".into())].into(),
                    // },
                },
            ),
            // (
            //     "nested fields",
            //     TestCase {
            //         path: LookupBuf::from_str(".foo.bar").unwrap(),
            //         kind: Kind::regex().or_null(),
            //         meaning: Some("foobar"),
            //         want: Definition {
            //             collection: BTreeMap::from([(
            //                 "foo".into(),
            //                 Kind::object(BTreeMap::from([("bar".into(), Kind::regex().or_null())])),
            //             )])
            //             .into(),
            //             meaning: [(
            //                 "foobar".to_owned(),
            //                 LookupBuf::from_str(".foo.bar").unwrap().into(),
            //             )]
            //             .into(),
            //         },
            //     },
            // ),
            // (
            //     "no meaning",
            //     TestCase {
            //         path: "foo".into(),
            //         kind: Kind::boolean(),
            //         meaning: None,
            //         want: Definition {
            //             collection: BTreeMap::from([("foo".into(), Kind::boolean())]).into(),
            //             meaning: BTreeMap::default(),
            //         },
            //     },
            // ),
        ]) {
            let mut got = Definition::empty_kind(Kind::any_object(), []);
            got = got.with_field(path, kind, meaning);

            assert_eq!(got, want, "{}", title);
        }
    }

    // #[test]
    // fn test_optional_field() {
    //     struct TestCase {
    //         path: LookupBuf,
    //         kind: Kind,
    //         meaning: Option<&'static str>,
    //         want: Definition,
    //     }
    //
    //     for (
    //         title,
    //         TestCase {
    //             path,
    //             kind,
    //             meaning,
    //             want,
    //         },
    //     ) in HashMap::from([
    //         (
    //             "simple",
    //             TestCase {
    //                 path: "foo".into(),
    //                 kind: Kind::boolean(),
    //                 meaning: Some("foo_meaning"),
    //                 want: Definition {
    //                     collection: BTreeMap::from([("foo".into(), Kind::boolean().or_null())])
    //                         .into(),
    //                     meaning: [("foo_meaning".to_owned(), "foo".into())].into(),
    //                 },
    //             },
    //         ),
    //         (
    //             "nested fields",
    //             TestCase {
    //                 path: LookupBuf::from_str(".foo.bar").unwrap(),
    //                 kind: Kind::regex().or_null(),
    //                 meaning: Some("foobar"),
    //                 want: Definition {
    //                     collection: BTreeMap::from([(
    //                         "foo".into(),
    //                         Kind::object(BTreeMap::from([("bar".into(), Kind::regex().or_null())])),
    //                     )])
    //                     .into(),
    //                     meaning: [(
    //                         "foobar".to_owned(),
    //                         LookupBuf::from_str(".foo.bar").unwrap().into(),
    //                     )]
    //                     .into(),
    //                 },
    //             },
    //         ),
    //         (
    //             "no meaning",
    //             TestCase {
    //                 path: "foo".into(),
    //                 kind: Kind::boolean(),
    //                 meaning: None,
    //                 want: Definition {
    //                     collection: BTreeMap::from([("foo".into(), Kind::boolean().or_null())])
    //                         .into(),
    //                     meaning: BTreeMap::default(),
    //                 },
    //             },
    //         ),
    //     ]) {
    //         let mut got = Definition::empty();
    //         got = got.optional_field(path, kind, meaning);
    //
    //         assert_eq!(got, want, "{}", title);
    //     }
    // }
    //
    // #[test]
    // fn test_unknown_fields() {
    //     let want = Definition {
    //         collection: Collection::from_unknown(Kind::bytes().or_integer()),
    //         meaning: BTreeMap::default(),
    //     };
    //
    //     let mut got = Definition::empty();
    //     got = got.unknown_fields(Kind::boolean());
    //     got = got.unknown_fields(Kind::bytes().or_integer());
    //
    //     assert_eq!(got, want);
    // }
    //
    // #[test]
    // #[allow(clippy::too_many_lines)]
    // fn test_merge() {
    //     struct TestCase {
    //         this: Definition,
    //         other: Definition,
    //         want: Definition,
    //     }
    //
    //     for (title, TestCase { this, other, want }) in HashMap::from([
    //         (
    //             "equal definitions",
    //             TestCase {
    //                 this: Definition {
    //                     collection: Collection::from(BTreeMap::from([(
    //                         "foo".into(),
    //                         Kind::boolean().or_null(),
    //                     )])),
    //                     meaning: BTreeMap::from([("foo_meaning".to_owned(), "foo".into())]),
    //                 },
    //                 other: Definition {
    //                     collection: Collection::from(BTreeMap::from([(
    //                         "foo".into(),
    //                         Kind::boolean().or_null(),
    //                     )])),
    //                     meaning: BTreeMap::from([("foo_meaning".to_owned(), "foo".into())]),
    //                 },
    //                 want: Definition {
    //                     collection: Collection::from(BTreeMap::from([(
    //                         "foo".into(),
    //                         Kind::boolean().or_null(),
    //                     )])),
    //                     meaning: BTreeMap::from([("foo_meaning".to_owned(), "foo".into())]),
    //                 },
    //             },
    //         ),
    //         (
    //             "this optional, other required",
    //             TestCase {
    //                 this: Definition {
    //                     collection: Collection::from(BTreeMap::from([(
    //                         "foo".into(),
    //                         Kind::boolean().or_null(),
    //                     )])),
    //                     meaning: BTreeMap::default(),
    //                 },
    //                 other: Definition {
    //                     collection: Collection::from(BTreeMap::from([(
    //                         "foo".into(),
    //                         Kind::boolean(),
    //                     )])),
    //                     meaning: BTreeMap::default(),
    //                 },
    //                 want: Definition {
    //                     collection: Collection::from(BTreeMap::from([(
    //                         "foo".into(),
    //                         Kind::boolean().or_null(),
    //                     )])),
    //                     meaning: BTreeMap::default(),
    //                 },
    //             },
    //         ),
    //         (
    //             "this required, other optional",
    //             TestCase {
    //                 this: Definition {
    //                     collection: Collection::from(BTreeMap::from([(
    //                         "foo".into(),
    //                         Kind::boolean(),
    //                     )])),
    //                     meaning: BTreeMap::default(),
    //                 },
    //                 other: Definition {
    //                     collection: Collection::from(BTreeMap::from([(
    //                         "foo".into(),
    //                         Kind::boolean().or_null(),
    //                     )])),
    //                     meaning: BTreeMap::default(),
    //                 },
    //                 want: Definition {
    //                     collection: Collection::from(BTreeMap::from([(
    //                         "foo".into(),
    //                         Kind::boolean().or_null(),
    //                     )])),
    //                     meaning: BTreeMap::default(),
    //                 },
    //             },
    //         ),
    //         (
    //             "this required, other required",
    //             TestCase {
    //                 this: Definition {
    //                     collection: Collection::from(BTreeMap::from([(
    //                         "foo".into(),
    //                         Kind::boolean(),
    //                     )])),
    //                     meaning: BTreeMap::default(),
    //                 },
    //                 other: Definition {
    //                     collection: Collection::from(BTreeMap::from([(
    //                         "foo".into(),
    //                         Kind::boolean(),
    //                     )])),
    //                     meaning: BTreeMap::default(),
    //                 },
    //                 want: Definition {
    //                     collection: Collection::from(BTreeMap::from([(
    //                         "foo".into(),
    //                         Kind::boolean(),
    //                     )])),
    //                     meaning: BTreeMap::default(),
    //                 },
    //             },
    //         ),
    //         (
    //             "same meaning, pointing to different paths",
    //             TestCase {
    //                 this: Definition {
    //                     collection: Collection::from(BTreeMap::from([(
    //                         "foo".into(),
    //                         Kind::boolean(),
    //                     )])),
    //                     meaning: BTreeMap::from([(
    //                         "foo".into(),
    //                         MeaningPointer::Valid("foo".into()),
    //                     )]),
    //                 },
    //                 other: Definition {
    //                     collection: Collection::from(BTreeMap::from([(
    //                         "foo".into(),
    //                         Kind::boolean(),
    //                     )])),
    //                     meaning: BTreeMap::from([(
    //                         "foo".into(),
    //                         MeaningPointer::Valid("bar".into()),
    //                     )]),
    //                 },
    //                 want: Definition {
    //                     collection: Collection::from(BTreeMap::from([(
    //                         "foo".into(),
    //                         Kind::boolean(),
    //                     )])),
    //                     meaning: BTreeMap::from([(
    //                         "foo".into(),
    //                         MeaningPointer::Invalid(BTreeSet::from(["foo".into(), "bar".into()])),
    //                     )]),
    //                 },
    //             },
    //         ),
    //         (
    //             "same meaning, pointing to same path",
    //             TestCase {
    //                 this: Definition {
    //                     collection: Collection::from(BTreeMap::from([(
    //                         "foo".into(),
    //                         Kind::boolean(),
    //                     )])),
    //                     meaning: BTreeMap::from([(
    //                         "foo".into(),
    //                         MeaningPointer::Valid("foo".into()),
    //                     )]),
    //                 },
    //                 other: Definition {
    //                     collection: Collection::from(BTreeMap::from([(
    //                         "foo".into(),
    //                         Kind::boolean(),
    //                     )])),
    //                     meaning: BTreeMap::from([(
    //                         "foo".into(),
    //                         MeaningPointer::Valid("foo".into()),
    //                     )]),
    //                 },
    //                 want: Definition {
    //                     collection: Collection::from(BTreeMap::from([(
    //                         "foo".into(),
    //                         Kind::boolean(),
    //                     )])),
    //                     meaning: BTreeMap::from([(
    //                         "foo".into(),
    //                         MeaningPointer::Valid("foo".into()),
    //                     )]),
    //                 },
    //             },
    //         ),
    //     ]) {
    //         let got = this.merge(other);
    //
    //         assert_eq!(got, want, "{}", title);
    //     }
    // }
}
