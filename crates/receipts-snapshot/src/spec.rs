//! Dataset definitions: which Socrata fields we fetch, what we expect their
//! types to be (CR-01), and how they map to snapshot columns.

use receipts_core::ColumnType;

/// How a snapshot column is derived from source fields. Each kind comes with
/// its own cleaning rules (`docs/snapshot/cleaning-rules.md`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// The source primary key: a positive base-10 integer. Required (CR-02).
    Key,
    /// The scope and sort timestamp. Required (CR-05, CR-12).
    CreatedTimestamp,
    /// An optional timestamp (CR-06).
    Timestamp,
    /// Dictionary-encoded text (CR-07, CR-08).
    Text,
    /// A point built from two decimal fields: `source_fields = [lat, lon]` (CR-09, CR-10).
    LatLon,
}

impl Kind {
    pub fn column_type(self) -> ColumnType {
        match self {
            Kind::Key => ColumnType::I64,
            Kind::CreatedTimestamp | Kind::Timestamp => ColumnType::Timestamp,
            Kind::Text => ColumnType::DictUtf8,
            Kind::LatLon => ColumnType::Geo,
        }
    }

    pub fn nullable(self) -> bool {
        !matches!(self, Kind::Key | Kind::CreatedTimestamp)
    }

    /// Socrata `dataTypeName`s we accept for each source field (CR-01).
    fn accepted_source_types(self) -> &'static [&'static str] {
        match self {
            Kind::Key => &["text", "number"],
            Kind::CreatedTimestamp | Kind::Timestamp => &["calendar_date"],
            Kind::Text => &["text"],
            Kind::LatLon => &["number"],
        }
    }
}

#[derive(Clone, Debug)]
pub struct ColumnSpec {
    pub name: &'static str,
    pub kind: Kind,
    pub source_fields: &'static [&'static str],
    /// Plain English, shown in the UI.
    pub description: &'static str,
}

#[derive(Clone, Debug)]
pub struct DatasetSpec {
    pub source_id: u16,
    pub dataset: &'static str,
    pub portal: &'static str,
    pub dataset_id: &'static str,
    pub source_url: &'static str,
    pub terms_url: &'static str,
    /// Human-readable scope template; `{from}` and `{to}` are dates.
    pub scope_sentence: &'static str,
    pub columns: &'static [ColumnSpec],
    pub excluded: &'static [(&'static str, &'static str)],
}

impl DatasetSpec {
    pub fn key_column(&self) -> usize {
        self.single(Kind::Key)
    }

    pub fn created_column(&self) -> usize {
        self.single(Kind::CreatedTimestamp)
    }

    fn single(&self, kind: Kind) -> usize {
        let mut it = self
            .columns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.kind == kind);
        let (i, _) = it
            .next()
            .expect("spec must have one column of each required kind");
        assert!(
            it.next().is_none(),
            "spec has more than one {kind:?} column"
        );
        i
    }

    pub fn key_field(&self) -> &'static str {
        self.columns[self.key_column()].source_fields[0]
    }

    pub fn created_field(&self) -> &'static str {
        self.columns[self.created_column()].source_fields[0]
    }

    /// Source fields to `$select`, in spec order.
    pub fn source_fields(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.columns
            .iter()
            .flat_map(|c| c.source_fields.iter().copied())
    }

    /// `(field, accepted Socrata types)` for every selected field.
    pub fn expected_types(
        &self,
    ) -> impl Iterator<Item = (&'static str, &'static [&'static str])> + '_ {
        self.columns.iter().flat_map(|c| {
            let accepted = c.kind.accepted_source_types();
            c.source_fields.iter().map(move |f| (*f, accepted))
        })
    }
}

const fn col(
    name: &'static str,
    kind: Kind,
    source_fields: &'static [&'static str],
    description: &'static str,
) -> ColumnSpec {
    ColumnSpec {
        name,
        kind,
        source_fields,
        description,
    }
}

pub static NYC_311: DatasetSpec = DatasetSpec {
    source_id: 1,
    dataset: "NYC 311 Service Requests",
    portal: "data.cityofnewyork.us",
    dataset_id: "erm2-nwe9",
    source_url: "https://data.cityofnewyork.us/Social-Services/311-Service-Requests-from-2010-to-Present/erm2-nwe9",
    terms_url: "https://opendata.cityofnewyork.us/overview/#termsofuse",
    scope_sentence: "311 service requests created from {from} up to (not including) {to}, NYC local time.",
    columns: &[
        col(
            "unique_key",
            Kind::Key,
            &["unique_key"],
            "The city's ID for this service request.",
        ),
        col(
            "created_date",
            Kind::CreatedTimestamp,
            &["created_date"],
            "When the request was made (NYC local time).",
        ),
        col(
            "closed_date",
            Kind::Timestamp,
            &["closed_date"],
            "When the city marked the request closed (NYC local time), exactly as published.",
        ),
        col(
            "agency",
            Kind::Text,
            &["agency"],
            "The city agency the request was routed to.",
        ),
        col(
            "complaint_type",
            Kind::Text,
            &["complaint_type"],
            "The kind of problem reported.",
        ),
        col(
            "descriptor",
            Kind::Text,
            &["descriptor"],
            "More detail on the kind of problem.",
        ),
        col(
            "location_type",
            Kind::Text,
            &["location_type"],
            "The type of place, such as a residential building or a street.",
        ),
        col(
            "incident_zip",
            Kind::Text,
            &["incident_zip"],
            "ZIP code of the problem, as published.",
        ),
        col(
            "borough",
            Kind::Text,
            &["borough"],
            "Borough the request was about. 'Unspecified' is the city's own label for requests without one.",
        ),
        col(
            "community_board",
            Kind::Text,
            &["community_board"],
            "Community board district, such as '12 MANHATTAN'.",
        ),
        col(
            "status",
            Kind::Text,
            &["status"],
            "Status of the request when the data was fetched.",
        ),
        col(
            "channel",
            Kind::Text,
            &["open_data_channel_type"],
            "How the request was made: phone, online, mobile app, and so on.",
        ),
        col(
            "location",
            Kind::LatLon,
            &["latitude", "longitude"],
            "Where the problem was reported, as latitude/longitude (stored to about 0.3 m).",
        ),
    ],
    excluded: &[
        (
            "resolution_description",
            "Free text; large and high-cardinality.",
        ),
        (
            "resolution_action_updated_date",
            "Not needed for v1 claims.",
        ),
        ("incident_address", "Free text address."),
        ("street_name", "Free text address."),
        ("cross_street_1", "Free text address."),
        ("cross_street_2", "Free text address."),
        ("intersection_street_1", "Free text address."),
        ("intersection_street_2", "Free text address."),
        (
            "address_type",
            "Describes the address fields, which are excluded.",
        ),
        (
            "city",
            "Redundant with borough and ZIP; inconsistently filled.",
        ),
        ("landmark", "Free text."),
        ("facility_type", "Sparse, domain-specific."),
        ("due_date", "Sparse."),
        ("agency_name", "Redundant with agency."),
        ("bbl", "Parcel ID; sparse and not needed for v1."),
        (
            "x_coordinate_state_plane",
            "Redundant with latitude/longitude.",
        ),
        (
            "y_coordinate_state_plane",
            "Redundant with latitude/longitude.",
        ),
        ("park_facility_name", "Sparse, domain-specific."),
        ("park_borough", "Redundant with borough."),
        ("vehicle_type", "Sparse, domain-specific."),
        ("taxi_company_borough", "Sparse, domain-specific."),
        ("taxi_pick_up_location", "Sparse, domain-specific."),
        ("bridge_highway_name", "Sparse, domain-specific."),
        ("bridge_highway_direction", "Sparse, domain-specific."),
        ("road_ramp", "Sparse, domain-specific."),
        ("bridge_highway_segment", "Sparse, domain-specific."),
        ("location", "Redundant with latitude/longitude."),
    ],
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nyc_311_is_well_formed() {
        let s = &NYC_311;
        assert_eq!(s.key_field(), "unique_key");
        assert_eq!(s.created_field(), "created_date");
        assert_eq!(s.columns.len(), 13);
        for c in s.columns {
            let expected = if c.kind == Kind::LatLon { 2 } else { 1 };
            assert_eq!(c.source_fields.len(), expected, "{}", c.name);
        }
        let selected: Vec<_> = s.source_fields().collect();
        for (field, _) in s.excluded {
            assert!(
                !selected.contains(field),
                "{field} both selected and excluded"
            );
        }
    }
}
