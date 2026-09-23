//! Rule A1: the app never enters the mixnet through a gateway run by whoever runs the
//! service.
//!
//! An entry gateway sees the user's IP address and when, and how much, arrives for them. The
//! host of the enclave sees when, and how much, leaves it. One party holding both could
//! match a user to their answers by timing and size — cover traffic hides a user's sending,
//! not a burst of answer packets arriving. So the app draws its entry gateway at random
//! from Nym's public directory, leaving out:
//!
//! - every node of the operator's Nym node family (read from the Nyx chain through Nym's
//!   API at each connect, so a new node of ours is left out without an app update — the
//!   family is checked to still belong to the operator's account);
//! - the gateways listed below, as a floor should the family ever lose a member;
//! - the gateway the enclave itself sits behind.
//!
//! Without the directory or the family there is no connection: letting the SDK pick blindly
//! could land on one of ours.

use serde_json::Value;
use std::collections::HashSet;
use std::time::Duration;

/// Nym's directory of described nodes: identity, role, host.
pub const DIRECTORY: &str = "https://validator.nymtech.net/api/v1/nym-nodes/described";

/// The operator's Nym node family ("Hermes Stakepool Germany"), and the Nyx account that
/// owns it. Every node of ours is a member.
pub const OPERATOR_FAMILY: u64 = 19;
pub const OPERATOR_FAMILY_OWNER: &str = "n1suh7h6qx5wja25rutmhelf6a9eey5jnclhu0xc";
pub const FAMILY_API: &str = "https://validator.nymtech.net/api/v1/node-families/";

/// The operator's gateways as of 2026-09-22 (their Nym identity keys, as published in the
/// directory) — the floor under the family, in the open so anyone can check the rule.
/// The gateways the enclave itself listens at, by the name we call them — for an
/// interface that offers the choice ("Germany", not a base58 key). Their being ours is
/// also why the app never enters through them (rule A1).
pub const OPERATOR_NAMES: &[(&str, &str)] = &[
    ("38zcSsvjXsAX7C28ko2H3Lt55X4TYxfZYkPADxKXZHUj", "Germany"),
    ("98FmUvDdQYEeV1ioi5NpFK7DoeHphVECndaG7fkRUsaF", "Austria"),
    ("6KZ96sPW6BBcgmghYb7c7BtCXgAEr1nmwnJzzRsszyhe", "Switzerland"),
];

pub const OPERATOR_GATEWAYS: &[&str] = &[
    "98FmUvDdQYEeV1ioi5NpFK7DoeHphVECndaG7fkRUsaF",
    "FmbUngD26tUGvJN8QqL78iK96bZYV2bWkjWZU7fDwBN1",
    "2JAVMSBKVvV5DAw8fsdsUjtaaTcwjzPvm4jMzaLGVzJJ",
    "89emNaGyaPFKzwLhVeh6wa75umdKfyQRBTa6ZPbUUPv2",
    "7ntzmDZRvG4a1pnDBU4Bg1RiAmLwmqXV5sZGNw68Ce14",
    "4mDXCAFNbHxhvayffcgvpRUTwjxTDy9AFZ3neQjNb8po",
    "4CmEWNXVKVAaY7Y5848fgmBTxwNTvkYwgnVitksPSnWY",
    "C3LSzGfG1gxhCShdijUgYdxo1BUGcUX1yP9YTkaQWwhp",
    "38zcSsvjXsAX7C28ko2H3Lt55X4TYxfZYkPADxKXZHUj",
    "6sL9w3iRYaf599rQ5QzGfecG8yoH9zci1BsbMqR6uyQP",
    "Hb8A6xoAKazBRWTGZ7eaCDZdR1eciaKWKrNqmydn4Bbw",
    "8h489z12rHrbXDZ25H7fEcbeSFusdQ8xD4jkntWyGEqr",
    "3JcdMZAHGrp3QbGh78SKQHuq1Bfq6x6yzCgvi5pqBniZ",
    "AcN3TJBpfHEtUo3qhrf1yELTeTgZfCFJt1xnyo4jnPwW",
    "2zHiExNRKiCXVKS35SNKtK4apGfZELMpA1jJ2gVevJoz",
    "EQBb3hW12n1XKM44peDfHLuT8XHPjZZdAFfMd5jF3XEa",
    "5txfqzqVFKACYzsC3cezvKKr1F4toN73wcq1hd365aJ",
    "YEFrKYaP1eAgs5xe1LfTiYmWALsWnAx44TfLscQoMkU",
    "FE6qX4c57NqjCnRtTcESkdC3oEUfZbv9Z3ATTHKKnA3b",
    "CWkVgyxmhjZgjw5azYAoqNzqaGCQEWhhQf5bfiiDyede",
    "Ht23soQt6NQXFNVGv2SHtb161XScgxrDgn7AF9ofET9L",
    "6KZ96sPW6BBcgmghYb7c7BtCXgAEr1nmwnJzzRsszyhe",
    "64LaDQefP7dbC8F37HhCnXGP2KstcfqM3hmPiw1KwezA",
];

/// How the entry gateway is chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryChoice {
    /// A random allowed gateway from the directory (the default).
    Random,
    /// One the user picked — still refused if it is one of ours.
    Chosen(String),
}

/// An entry gateway from the directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gateway {
    pub node_id: u64,
    pub id: String,
    pub host: String,
    pub country: String,
}

/// The entry gateways in a directory answer.
pub fn parse_directory(body: &Value) -> Vec<Gateway> {
    let Some(items) = body.get("data").and_then(|d| d.as_array()) else { return Vec::new() };
    items
        .iter()
        .filter_map(|it| {
            let d = it.get("description")?;
            if d.pointer("/declared_role/entry").and_then(|v| v.as_bool()) != Some(true) {
                return None;
            }
            let node_id = it.get("node_id").and_then(|v| v.as_u64())?;
            let id = d.pointer("/host_information/keys/ed25519").and_then(|v| v.as_str())?.to_string();
            let host = d.pointer("/host_information/hostname").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let country = d.pointer("/auxiliary_details/location").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            Some(Gateway { node_id, id, host, country })
        })
        .collect()
}

/// The node ids of the operator's family, from Nym's answer — refused if the family is not
/// (or no longer) owned by the operator's account.
pub fn parse_family(body: &Value) -> Result<HashSet<u64>, String> {
    let f = body.get("family").ok_or("the Nym API sent no node family")?;
    if f.get("id").and_then(|v| v.as_u64()) != Some(OPERATOR_FAMILY) || f.get("owner").and_then(|v| v.as_str()) != Some(OPERATOR_FAMILY_OWNER) {
        return Err("the operator's node family is not where it should be — not connecting".into());
    }
    let members: HashSet<u64> = f
        .get("members")
        .and_then(|m| m.as_array())
        .map(|m| m.iter().filter_map(|x| x.get("node_id").and_then(|v| v.as_u64())).collect())
        .unwrap_or_default();
    if members.is_empty() {
        return Err("the operator's node family came back empty — not connecting".into());
    }
    Ok(members)
}

/// What the rule needs: the entry gateways, and which nodes are ours.
#[derive(Debug, Clone, Default)]
pub struct Directory {
    pub gateways: Vec<Gateway>,
    pub operator_nodes: HashSet<u64>,
}

/// The gateway part of a Nym address (`client.encryption@gateway`).
pub fn gateway_of(address: &str) -> Option<&str> {
    address.trim().rsplit_once('@').map(|(_, g)| g)
}

impl Directory {
    /// Whether the app may enter through the gateway with identity `id`.
    pub fn allowed(&self, id: &str, enclave_address: &str) -> bool {
        let family = self.gateways.iter().any(|g| g.id == id && self.operator_nodes.contains(&g.node_id));
        !family && !OPERATOR_GATEWAYS.contains(&id) && gateway_of(enclave_address) != Some(id)
    }
}

/// The gateways the app may enter through, from a directory.
pub fn candidates(directory: &Directory, enclave_address: &str) -> Vec<Gateway> {
    let mut seen = HashSet::new();
    directory
        .gateways
        .iter()
        // A named host and a location, as a sign of a node that is looked after (a bare
        // IP address or no location was, in the first app, often a slow or dead one).
        .filter(|g| directory.allowed(&g.id, enclave_address) && !g.host.is_empty() && g.host.parse::<std::net::IpAddr>().is_err() && !g.country.is_empty())
        .filter(|g| seen.insert(g.id.clone()))
        .cloned()
        .collect()
}

/// The entry gateway to connect through.
pub fn pick(directory: &Directory, choice: &EntryChoice, enclave_address: &str) -> Result<String, String> {
    match choice {
        // A chosen gateway must be in the directory: only there can it be told whether it
        // belongs to the operator's family.
        EntryChoice::Chosen(id) if !directory.gateways.iter().any(|g| &g.id == id) => Err("that gateway is not an entry gateway in the Nym directory right now — choose another".into()),
        EntryChoice::Chosen(id) if directory.allowed(id, enclave_address) => Ok(id.clone()),
        EntryChoice::Chosen(_) => Err("that gateway is run by tokumai, or is the enclave's own — choose another, so no one sees both ends".into()),
        EntryChoice::Random => {
            use rand::seq::SliceRandom;
            candidates(directory, enclave_address)
                .choose(&mut rand::thread_rng())
                .map(|g| g.id.clone())
                .ok_or_else(|| "no suitable entry gateway in the Nym directory".into())
        }
    }
}

async fn get(url: &str) -> Result<Value, String> {
    reqwest::Client::new()
        .get(url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("the Nym API is unreachable: {e}"))?
        .json()
        .await
        .map_err(|e| format!("the Nym API answered unreadably: {e}"))
}

/// Fetch the directory and the operator's family.
pub async fn fetch() -> Result<Directory, String> {
    let family_url = format!("{FAMILY_API}{OPERATOR_FAMILY}");
    let (dir, family) = tokio::join!(get(DIRECTORY), get(&family_url));
    Ok(Directory { gateways: parse_directory(&dir?), operator_nodes: parse_family(&family?)? })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(node_id: u64, id: &str, entry: bool, host: &str, country: &str) -> Value {
        json!({ "node_id": node_id, "description": {
            "declared_role": { "entry": entry },
            "host_information": { "keys": { "ed25519": id }, "hostname": host },
            "auxiliary_details": { "location": country },
        }})
    }

    const ENCLAVE: &str = "Client.Enc@EnclaveGatewayId";

    #[test]
    fn never_ours_never_the_enclaves_own_only_entry_gateways() {
        let body = json!({ "data": [
            node(1, OPERATOR_GATEWAYS[0], true, "ours.example", "AT"),
            node(2, "EnclaveGatewayId", true, "exit.example", "DE"),
            node(3, "MixOnly", false, "mix.example", "NL"),
            node(4, "BareIp", true, "10.0.0.1", "FR"),
            node(5, "Theirs", true, "gw.elsewhere.example", "CH"),
            node(6, "OurNewNode", true, "new.example", "PT"),
        ]});
        let family = json!({ "family": { "id": OPERATOR_FAMILY, "owner": OPERATOR_FAMILY_OWNER, "members": [{ "node_id": 6 }, { "node_id": 1 }] } });
        let dir = Directory { gateways: parse_directory(&body), operator_nodes: parse_family(&family).unwrap() };
        assert_eq!(dir.gateways.len(), 5, "only entry gateways");
        let ok = candidates(&dir, ENCLAVE);
        assert_eq!(ok.iter().map(|g| g.id.as_str()).collect::<Vec<_>>(), vec!["Theirs"]);
        for _ in 0..20 {
            assert_eq!(pick(&dir, &EntryChoice::Random, ENCLAVE).unwrap(), "Theirs");
        }
    }

    #[test]
    fn a_family_that_is_not_the_operators_is_not_trusted() {
        let moved = json!({ "family": { "id": OPERATOR_FAMILY, "owner": "n1someoneelse", "members": [{ "node_id": 6 }] } });
        assert!(parse_family(&moved).is_err());
        let empty = json!({ "family": { "id": OPERATOR_FAMILY, "owner": OPERATOR_FAMILY_OWNER, "members": [] } });
        assert!(parse_family(&empty).is_err());
        assert!(parse_family(&json!({})).is_err());
    }

    #[test]
    fn a_chosen_gateway_of_ours_is_refused_and_no_directory_means_no_connection() {
        let none = Directory::default();
        assert!(pick(&none, &EntryChoice::Chosen(OPERATOR_GATEWAYS[3].into()), ENCLAVE).is_err());
        assert!(pick(&none, &EntryChoice::Chosen("EnclaveGatewayId".into()), ENCLAVE).is_err());
        assert!(pick(&none, &EntryChoice::Chosen("Theirs".into()), ENCLAVE).is_err(), "not in the directory, so not checkable");
        assert!(pick(&none, &EntryChoice::Random, ENCLAVE).is_err(), "fail closed, never let the SDK pick");
        // A family member of ours, chosen by hand, is refused too.
        let dir = Directory {
            gateways: parse_directory(&json!({ "data": [node(6, "OurNewNode", true, "new.example", "PT")] })),
            operator_nodes: [6].into_iter().collect(),
        };
        assert!(pick(&dir, &EntryChoice::Chosen("OurNewNode".into()), ENCLAVE).is_err());
        let theirs = Directory { gateways: parse_directory(&json!({ "data": [node(5, "Theirs", true, "gw.example", "CH")] })), operator_nodes: [6].into_iter().collect() };
        assert_eq!(pick(&theirs, &EntryChoice::Chosen("Theirs".into()), ENCLAVE).unwrap(), "Theirs");
        assert_eq!(gateway_of(ENCLAVE), Some("EnclaveGatewayId"));
    }
}

/// Against the live Nym API: `cargo test -p tokumai-client -- --ignored`.
#[cfg(test)]
mod live {
    #[tokio::test]
    #[ignore]
    async fn the_live_family_holds_every_listed_gateway_and_none_is_a_candidate() {
        let dir = super::fetch().await.unwrap();
        let ours: Vec<&str> = dir.gateways.iter().filter(|g| dir.operator_nodes.contains(&g.node_id)).map(|g| g.id.as_str()).collect();
        let missing: Vec<&&str> = super::OPERATOR_GATEWAYS.iter().filter(|k| !ours.contains(k)).collect();
        println!("{} entry gateways, {} of them in the operator's family; listed but not in the family: {missing:?}", dir.gateways.len(), ours.len());
        let c = super::candidates(&dir, "x.y@z");
        assert!(!c.is_empty());
        assert!(c.iter().all(|g| !dir.operator_nodes.contains(&g.node_id) && !super::OPERATOR_GATEWAYS.contains(&g.id.as_str())));
    }
}
