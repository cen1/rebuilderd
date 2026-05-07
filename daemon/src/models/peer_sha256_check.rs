use crate::schema::peer_sha256_checks;
use chrono::NaiveDateTime;
use diesel::prelude::*;

#[derive(Queryable, Selectable, Debug)]
#[diesel(table_name = peer_sha256_checks)]
#[diesel(check_for_backend(diesel::sqlite::Sqlite))]
pub struct PeerSha256Check {
    pub id: i32,
    pub peer_rebuilder_id: i32,
    pub binary_name: String,
    pub binary_version: String,
    pub peer_build_id: Option<i32>,
    pub checked_at: NaiveDateTime,
    /// Peer's BuildStatus serialized as string (e.g. "GOOD", "BAD").
    pub peer_status: Option<String>,
}

/// Used for INSERT ... ON CONFLICT DO UPDATE in the disagreement cache.
#[derive(Insertable, AsChangeset, Debug)]
#[diesel(table_name = peer_sha256_checks)]
#[diesel(treat_none_as_null = true)]
pub struct UpsertPeerSha256Check {
    pub peer_rebuilder_id: i32,
    pub binary_name: String,
    pub binary_version: String,
    pub peer_build_id: Option<i32>,
    pub checked_at: NaiveDateTime,
    pub peer_status: Option<String>,
}
