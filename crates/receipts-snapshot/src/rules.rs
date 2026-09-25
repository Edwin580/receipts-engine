//! The cleaning rules of `docs/snapshot/cleaning-rules.md`. IDs are stable;
//! changing any rule's behaviour bumps [`RULES_VERSION`].

use serde::{Deserialize, Serialize};

pub const RULES_VERSION: u32 = 1;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Fail,
    Reject,
    Null,
    Normalize,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Rule {
    SchemaDrift,
    InvalidKey,
    DuplicateIdentical,
    DuplicateConflicting,
    InvalidCreated,
    InvalidClosed,
    Trimmed,
    EmptyText,
    InvalidLocation,
    LocationToF32,
    FractionalSeconds,
    OutsideScope,
}

impl Rule {
    pub const ALL: [Rule; 12] = [
        Rule::SchemaDrift,
        Rule::InvalidKey,
        Rule::DuplicateIdentical,
        Rule::DuplicateConflicting,
        Rule::InvalidCreated,
        Rule::InvalidClosed,
        Rule::Trimmed,
        Rule::EmptyText,
        Rule::InvalidLocation,
        Rule::LocationToF32,
        Rule::FractionalSeconds,
        Rule::OutsideScope,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Rule::SchemaDrift => "CR-01",
            Rule::InvalidKey => "CR-02",
            Rule::DuplicateIdentical => "CR-03",
            Rule::DuplicateConflicting => "CR-04",
            Rule::InvalidCreated => "CR-05",
            Rule::InvalidClosed => "CR-06",
            Rule::Trimmed => "CR-07",
            Rule::EmptyText => "CR-08",
            Rule::InvalidLocation => "CR-09",
            Rule::LocationToF32 => "CR-10",
            Rule::FractionalSeconds => "CR-11",
            Rule::OutsideScope => "CR-12",
        }
    }

    pub fn action(self) -> Action {
        match self {
            Rule::SchemaDrift => Action::Fail,
            Rule::InvalidKey
            | Rule::DuplicateIdentical
            | Rule::DuplicateConflicting
            | Rule::InvalidCreated
            | Rule::OutsideScope => Action::Reject,
            Rule::InvalidClosed | Rule::EmptyText | Rule::InvalidLocation => Action::Null,
            Rule::Trimmed | Rule::LocationToF32 | Rule::FractionalSeconds => Action::Normalize,
        }
    }

    /// Whether each application is written to the cleaning log. CR-10 and
    /// CR-11 apply systematically and are only counted.
    pub fn logged(self) -> bool {
        matches!(
            self,
            Rule::InvalidClosed | Rule::Trimmed | Rule::EmptyText | Rule::InvalidLocation
        )
    }

    /// What `count` means in the manifest for this rule.
    pub fn counts(self) -> &'static str {
        match self.action() {
            Action::Fail => "failures (always 0 in a finished snapshot)",
            Action::Reject => "records rejected",
            _ if self.logged() => "values changed (one cleaning-log row each)",
            _ => "values affected (not logged individually)",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Rule::SchemaDrift => {
                "An expected source field is missing or has changed type: the build stops."
            }
            Rule::InvalidKey => {
                "The record has no usable ID (missing, or not a positive whole number without leading zeros)."
            }
            Rule::DuplicateIdentical => {
                "Another record has the same ID and identical values; one copy is kept."
            }
            Rule::DuplicateConflicting => {
                "Several records share an ID but disagree; all copies are rejected because none can be preferred."
            }
            Rule::InvalidCreated => "The creation time is missing or unreadable.",
            Rule::InvalidClosed => "The closed time is unreadable, so it is recorded as missing.",
            Rule::Trimmed => "Spaces at the start or end of a text value were removed.",
            Rule::EmptyText => "A text value was empty, so it is recorded as missing.",
            Rule::InvalidLocation => {
                "Only one coordinate is present, or a coordinate is unreadable, so the location is recorded as missing."
            }
            Rule::LocationToF32 => {
                "Coordinates are stored in single precision (at most about 0.3 m of rounding in NYC)."
            }
            Rule::FractionalSeconds => {
                "Times with fractions of a second are kept to the microsecond (no loss)."
            }
            Rule::OutsideScope => {
                "The creation time is outside the snapshot's date range, even though the API returned the record."
            }
        }
    }

    pub fn from_id(id: &str) -> Option<Rule> {
        Rule::ALL.into_iter().find(|r| r.id() == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_ordered() {
        let ids: Vec<_> = Rule::ALL.iter().map(|r| r.id()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(ids, sorted);
        for r in Rule::ALL {
            assert_eq!(Rule::from_id(r.id()), Some(r));
        }
    }
}
