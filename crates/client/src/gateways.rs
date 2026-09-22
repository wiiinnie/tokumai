//! Rule A1: the app never enters the mixnet through a gateway run by whoever runs the
//! service.
//!
//! An entry gateway sees the user's IP address and when, and how much, arrives for them. The
//! host of the enclave sees when, and how much, leaves it. One party holding both could
//! match a user to their answers by timing and size — cover traffic hides a user's sending,
//! not a burst of answer packets arriving. So the app draws its entry gateway at random
//! from Nym's public directory, leaving out every gateway listed below and the gateway the
//! enclave itself sits behind. Without a directory there is no connection: letting the SDK
//! pick blindly could land on one of ours.

use serde_json::Value;
use std::collections::HashSet;
use std::time::Duration;

/// Nym's directory of described nodes: identity, role, host.
pub const DIRECTORY: &str = "https://validator.nymtech.net/api/v1/nym-nodes/described";

/// The gateways run by the operator of the official tokumai service (their Nym identity
/// keys, as published in the directory). Kept here, in the open, so anyone can check the
/// rule; a new gateway of ours goes onto this list before it goes online.
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
            let id = d.pointer("/host_information/keys/ed25519").and_then(|v| v.as_str())?.to_string();
            let host = d.pointer("/host_information/hostname").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let country = d.pointer("/auxiliary_details/location").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            Some(Gateway { id, host, country })
        })
        .collect()
}

/// The gateway part of a Nym address (`client.encryption@gateway`).
pub fn gateway_of(address: &str) -> Option<&str> {
    address.trim().rsplit_once('@').map(|(_, g)| g)
}

/// Whether the app may enter through `id`, given the enclave's own address.
pub fn allowed(id: &str, enclave_address: &str) -> bool {
    !OPERATOR_GATEWAYS.contains(&id) && gateway_of(enclave_address) != Some(id)
}

/// The gateways the app may enter through, from a directory.
pub fn candidates(directory: &[Gateway], enclave_address: &str) -> Vec<Gateway> {
    let mut seen = HashSet::new();
    directory
        .iter()
        // A named host and a location, as a sign of a node that is looked after (a bare
        // IP address or no location was, in the first app, often a slow or dead one).
        .filter(|g| allowed(&g.id, enclave_address) && !g.host.is_empty() && g.host.parse::<std::net::IpAddr>().is_err() && !g.country.is_empty())
        .filter(|g| seen.insert(g.id.clone()))
        .cloned()
        .collect()
}

/// The entry gateway to connect through.
pub fn pick(directory: &[Gateway], choice: &EntryChoice, enclave_address: &str) -> Result<String, String> {
    match choice {
        EntryChoice::Chosen(id) if allowed(id, enclave_address) => Ok(id.clone()),
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

/// Fetch the directory.
pub async fn fetch() -> Result<Vec<Gateway>, String> {
    let body: Value = reqwest::Client::new()
        .get(DIRECTORY)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("the Nym directory is unreachable: {e}"))?
        .json()
        .await
        .map_err(|e| format!("the Nym directory answered unreadably: {e}"))?;
    Ok(parse_directory(&body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(id: &str, entry: bool, host: &str, country: &str) -> Value {
        json!({ "description": {
            "declared_role": { "entry": entry },
            "host_information": { "keys": { "ed25519": id }, "hostname": host },
            "auxiliary_details": { "location": country },
        }})
    }

    const ENCLAVE: &str = "Client.Enc@EnclaveGatewayId";

    #[test]
    fn never_ours_never_the_enclaves_own_only_entry_gateways() {
        let body = json!({ "data": [
            node(OPERATOR_GATEWAYS[0], true, "ours.example", "AT"),
            node("EnclaveGatewayId", true, "exit.example", "DE"),
            node("MixOnly", false, "mix.example", "NL"),
            node("BareIp", true, "10.0.0.1", "FR"),
            node("Theirs", true, "gw.elsewhere.example", "CH"),
        ]});
        let dir = parse_directory(&body);
        assert_eq!(dir.len(), 4, "only entry gateways");
        let ok = candidates(&dir, ENCLAVE);
        assert_eq!(ok.iter().map(|g| g.id.as_str()).collect::<Vec<_>>(), vec!["Theirs"]);
        for _ in 0..20 {
            assert_eq!(pick(&dir, &EntryChoice::Random, ENCLAVE).unwrap(), "Theirs");
        }
    }

    #[test]
    fn a_chosen_gateway_of_ours_is_refused_and_no_directory_means_no_connection() {
        assert!(pick(&[], &EntryChoice::Chosen(OPERATOR_GATEWAYS[3].into()), ENCLAVE).is_err());
        assert!(pick(&[], &EntryChoice::Chosen("EnclaveGatewayId".into()), ENCLAVE).is_err());
        assert_eq!(pick(&[], &EntryChoice::Chosen("Theirs".into()), ENCLAVE).unwrap(), "Theirs");
        assert!(pick(&[], &EntryChoice::Random, ENCLAVE).is_err(), "fail closed, never let the SDK pick");
        assert_eq!(gateway_of(ENCLAVE), Some("EnclaveGatewayId"));
    }
}
