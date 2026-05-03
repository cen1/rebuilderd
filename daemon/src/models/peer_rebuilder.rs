use crate::schema::peer_rebuilders;
use diesel::prelude::*;

#[derive(Identifiable, Queryable, Selectable, Clone, Debug)]
#[diesel(table_name = peer_rebuilders)]
#[diesel(check_for_backend(diesel::sqlite::Sqlite))]
pub struct PeerRebuilder {
    pub id: i32,
    pub url: String,
    pub distribution: String,
    pub architecture: String,
    /// Local release name (e.g. "sid"). Empty string = no release (Arch Linux).
    pub release: String,
    /// Release name the peer uses (e.g. "unstable"). Empty string = same as local.
    pub release_alias: String,
}

#[derive(Insertable, AsChangeset, Debug, Clone)]
#[diesel(table_name = peer_rebuilders)]
pub struct NewPeerRebuilder {
    pub url: String,
    pub distribution: String,
    pub architecture: String,
    /// Local release name. Empty string for "no release filter".
    pub release: String,
    /// Peer-side release name. Empty string = same as `release`.
    pub release_alias: String,
}
