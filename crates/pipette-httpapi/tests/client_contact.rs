//! Who reads a device's `contact_email`: the accounts that administer the
//! device, on every user read serving its record. The fleet grant reads the
//! rest of the record without it.

use anyhow::Context;
use axum::http::StatusCode;
use serde_json::{Value, json};
use tempfile::TempDir;

use pipette_plan_types::{ClientId, ClientOwner, OrgId, UserPermission};

use pipette_httpapi::state::AppState;

use pipette_web_testkit::*;

/// Two organizations' devices, all pending so the overviews highlight them,
/// and the accounts reading them.
struct Fleet {
    _dir: TempDir,
    state: AppState,
    acme: OrgId,
    rival: OrgId,
    /// acme's directly.
    ours: ClientId,
    /// An acme member's, so acme's through the membership.
    members: ClientId,
    unowned: ClientId,
    /// An account's in no organization, so no organization's.
    stray: ClientId,
    /// rival's.
    theirs: ClientId,
    /// Every read grant over clients and organizations, no device administration.
    reader: UserSigner,
    /// `manage_clients` in acme.
    org_admin: UserSigner,
    /// `manage_any_clients`.
    fleet_admin: UserSigner,
}

async fn fleet() -> anyhow::Result<Fleet> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;
    let acme = seed_org(&state, "acme").await?.org_id;
    let rival = seed_org(&state, "rival").await?.org_id;

    let (_, ours) = register_pending(&state).await?;
    own_client_by_org(&state, &ours, &acme).await?;
    let member = registered_user(&state, &[]).await?;
    set_user_org(&state, &member.1, Some(acme.clone())).await?;
    let (_, members) = register_pending(&state).await?;
    own_client(
        &state,
        &members,
        ClientOwner::User {
            user_id: member.1.clone(),
        },
    )
    .await?;
    let (_, unowned) = register_pending(&state).await?;
    let loner = registered_user(&state, &[]).await?;
    let (_, stray) = register_pending(&state).await?;
    own_client(
        &state,
        &stray,
        ClientOwner::User {
            user_id: loner.1.clone(),
        },
    )
    .await?;
    let (_, theirs) = register_pending(&state).await?;
    own_client_by_org(&state, &theirs, &rival).await?;

    let reader = registered_user(
        &state,
        &[UserPermission::ReadAnyClients, UserPermission::ReadAnyOrgs],
    )
    .await?;
    set_user_org(&state, &reader.1, Some(acme.clone())).await?;
    let org_admin = registered_user(
        &state,
        &[UserPermission::ManageClients, UserPermission::ReadAnyOrgs],
    )
    .await?;
    set_user_org(&state, &org_admin.1, Some(acme.clone())).await?;
    let fleet_admin = registered_user(
        &state,
        &[
            UserPermission::ManageAnyClients,
            UserPermission::ReadAnyOrgs,
        ],
    )
    .await?;
    Ok(Fleet {
        _dir: dir,
        state,
        acme,
        rival,
        ours,
        members,
        unowned,
        stray,
        theirs,
        reader,
        org_admin,
        fleet_admin,
    })
}

/// The `200` body `signer` reads at `uri`.
async fn read(state: &AppState, signer: &UserSigner, uri: &str) -> anyhow::Result<Value> {
    let (key, user_id, key_id) = signer;
    let resp = authed_user_get(state, key, user_id, key_id, uri).await?;
    anyhow::ensure!(resp.status() == StatusCode::OK, "{uri}: {}", resp.status());
    body_json(resp).await
}

/// Each listed device's id, and whether its row carries the address.
fn shown(rows: Option<&Value>) -> anyhow::Result<Vec<(String, bool)>> {
    let rows = rows
        .and_then(Value::as_array)
        .context("a listing carries its rows")?;
    rows.iter()
        .map(|row| {
            let client_id = row
                .get("client_id")
                .and_then(Value::as_str)
                .context("a row names its device")?;
            Ok((client_id.to_owned(), row.get("contact_email").is_some()))
        })
        .collect()
}

/// `GET /api/v1/clients` and `GET /api/v1/clients/{client_id}` carry the
/// address for the devices the reader administers: an organization's own,
/// through a member too, for `manage_clients`, every one for
/// `manage_any_clients`. The fleet grants alone read none.
#[tokio::test]
async fn test_the_fleet_reads_serve_contact_email_to_administrators_only() -> anyhow::Result<()> {
    let fleet = fleet().await?;
    let read_clients = registered_user(&fleet.state, &[UserPermission::ReadClients]).await?;
    let all = [
        &fleet.ours,
        &fleet.members,
        &fleet.unowned,
        &fleet.stray,
        &fleet.theirs,
    ];
    let cases = [
        ("read_clients", &read_clients, vec![]),
        ("read_any_clients", &fleet.reader, vec![]),
        (
            "manage_clients",
            &fleet.org_admin,
            vec![&fleet.ours, &fleet.members],
        ),
        ("manage_any_clients", &fleet.fleet_admin, all.to_vec()),
    ];
    for (who, signer, administered) in cases {
        let listing = read(&fleet.state, signer, "/api/v1/clients").await?;
        crate::api_schema::assert_conforms("ClientListRowPage", &listing)?;
        let mut rows = shown(listing.get("items"))?;
        rows.sort();
        let mut expected: Vec<(String, bool)> = all
            .iter()
            .map(|id| (id.to_string(), administered.contains(id)))
            .collect();
        expected.sort();
        assert_eq!(rows, expected, "{who}: {listing}");

        for client_id in all {
            let record = read(
                &fleet.state,
                signer,
                &format!("/api/v1/clients/{client_id}"),
            )
            .await?;
            crate::api_schema::assert_conforms("ClientDetail", &record)?;
            let administers = administered.contains(&client_id);
            assert_eq!(
                record.get("contact_email").is_some(),
                administers,
                "{who} on {client_id}: {record}"
            );
            assert_eq!(record["may_administer"], json!(administers), "{record}");
        }
    }
    Ok(())
}

/// `GET /api/v1/orgs/{org_id}/clients` and the overview's device highlights
/// carry the address for the organization's administrators only.
#[tokio::test]
async fn test_the_org_reads_serve_contact_email_to_administrators_only() -> anyhow::Result<()> {
    let fleet = fleet().await?;
    let cases = [
        ("reader", &fleet.reader, vec![]),
        ("manage_clients", &fleet.org_admin, vec![&fleet.acme]),
        (
            "manage_any_clients",
            &fleet.fleet_admin,
            vec![&fleet.acme, &fleet.rival],
        ),
    ];
    for (who, signer, administered) in cases {
        for (org_id, devices) in [
            (&fleet.acme, vec![&fleet.ours, &fleet.members]),
            (&fleet.rival, vec![&fleet.theirs]),
        ] {
            let administers = administered.contains(&org_id);
            let mut expected: Vec<(String, bool)> = devices
                .iter()
                .map(|id| (id.to_string(), administers))
                .collect();
            expected.sort();

            let listing = read(
                &fleet.state,
                signer,
                &format!("/api/v1/orgs/{org_id}/clients"),
            )
            .await?;
            let mut rows = shown(listing.get("items"))?;
            rows.sort();
            assert_eq!(rows, expected, "{who} on {org_id}: {listing}");

            let overview = read(
                &fleet.state,
                signer,
                &format!("/api/v1/orgs/{org_id}/overview"),
            )
            .await?;
            crate::api_schema::assert_conforms("OrgOverview", &overview)?;
            let mut rows = shown(overview.pointer("/pending_clients/first"))?;
            rows.sort();
            assert_eq!(rows, expected, "{who} on {org_id}: {overview}");
        }
    }
    Ok(())
}
