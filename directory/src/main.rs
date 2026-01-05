use anyhow::Result;
use chrono::{Duration, Utc};
use nym_sdk::mixnet::{self, MixnetMessageSender, Recipient, IncludedSurbs};
use scramble_shared::directory::{DirectoryRequest, DirectoryResponse, ServerEntry};
use scramble_shared::Provider;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool};
use std::collections::HashMap;
use std::io::{self, Write};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<()> {
    println!("🗂️  ScrambleAI Directory Service");
    println!("=================================\n");
    
    print!("Custom Gateway ID? (press ENTER for auto): ");
    io::stdout().flush()?;
    
    let mut choice = String::new();
    io::stdin().read_line(&mut choice)?;
    let choice = choice.trim();
    
    let gateway_id = if choice.is_empty() {
        println!("✅ Using automatic gateway selection\n");
        None
    } else if choice.len() > 40 {
        println!("✅ Using custom gateway: {}\n", choice);
        Some(choice.to_string())
    } else {
        println!("⚠️  Gateway ID too short, using auto\n");
        None
    };
    
    println!("📦 Setting up database...");
    let db = setup_database().await?;
    println!("✅ Database ready\n");
    
    // Track pending challenges with their providers
    let pending_providers: Arc<Mutex<HashMap<String, Vec<Provider>>>> = Arc::new(Mutex::new(HashMap::new()));
    
    println!("🚀 Initializing Nym Client...");
    let init_start = Instant::now();
    
    let mut builder = mixnet::MixnetClientBuilder::new_ephemeral();
    
    if let Some(gw_id) = gateway_id {
        builder = builder.request_gateway(gw_id);
    }
    
    let client = builder.build()?;
    println!("   ⏱️  Built: {:.2}s", init_start.elapsed().as_secs_f64());
    
    let mut nym_client = client.connect_to_mixnet().await?;
    println!("   ⏱️  Connected: {:.2}s", init_start.elapsed().as_secs_f64());
    
    let directory_address = nym_client.nym_address();
    
    println!("\n✅ Directory Ready!");
    println!("📍 Address: {}", directory_address);
    println!("🌐 Gateway: {}", directory_address.gateway());
    println!("\n⏳ Listening...\n");
    
    let nym_client = Arc::new(Mutex::new(nym_client));
    
    let db_clone = db.clone();
    tokio::spawn(async move {
        cleanup_task(db_clone).await;
    });
    
    let db_clone = db.clone();
    tokio::spawn(async move {
        status_printer(db_clone).await;
    });
    
    let nym_client_clone = nym_client.clone();
    
    loop {
        let mut client_lock = nym_client_clone.lock().await;
        
        if let Some(messages) = client_lock.wait_for_messages().await {
            drop(client_lock);
            
            for received in messages {
                let db = db.clone();
                let nym_client = nym_client.clone();
                let pending_providers = pending_providers.clone();
                
                tokio::spawn(async move {
                    if let Err(e) = handle_message(received, db, nym_client, pending_providers).await {
                        eprintln!("❌ Error: {}", e);
                    }
                });
            }
        } else {
            drop(client_lock);
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
    }
}

async fn setup_database() -> Result<SqlitePool> {
    let options = SqliteConnectOptions::from_str("sqlite::memory:")?.create_if_missing(true);
    let pool = SqlitePool::connect_with(options).await?;
    
    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS servers (
            address TEXT PRIMARY KEY,
            version TEXT NOT NULL,
            providers TEXT NOT NULL,
            registered_at TEXT NOT NULL,
            expires_at TEXT NOT NULL
        )"#,
    ).execute(&pool).await?;
    
    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS pending_challenges (
            challenge_id TEXT PRIMARY KEY,
            address TEXT NOT NULL,
            number INTEGER NOT NULL,
            created_at TEXT NOT NULL,
            expires_at TEXT NOT NULL
        )"#,
    ).execute(&pool).await?;
    
    Ok(pool)
}

async fn handle_message(
    received: nym_sdk::mixnet::ReconstructedMessage,
    db: SqlitePool,
    nym_client: Arc<Mutex<nym_sdk::mixnet::MixnetClient>>,
    pending_providers: Arc<Mutex<HashMap<String, Vec<Provider>>>>,
) -> Result<()> {
    let request: DirectoryRequest = serde_json::from_slice(&received.message)?;
    
    match request {
        DirectoryRequest::ListServers { reply_to } => {
            println!("📋 List servers request");
            handle_list_servers(reply_to, db, nym_client, &received).await?;
        }
        DirectoryRequest::Register { address, version, providers, reply_to } => {
            println!("\n═══════════════════════════════════════");
            println!("📝 REGISTRATION");
            println!("   Address: {}...", &address[..20]);
            println!("   Providers: {:?}", providers);
            println!("   Mode: {}", if reply_to.is_empty() { "Privacy (SURB)" } else { "Fast" });
            println!("═══════════════════════════════════════");
            handle_register(address, version, providers, reply_to, db, nym_client, &received, pending_providers).await?;
        }
        DirectoryRequest::ChallengeResponse { challenge_id, response } => {
            println!("🎯 Challenge response: {}", challenge_id);
            handle_challenge_response(challenge_id, response, db, nym_client, pending_providers).await?;
        }
    }
    
    Ok(())
}

async fn handle_list_servers(
    reply_to: Option<String>,
    db: SqlitePool,
    nym_client: Arc<Mutex<nym_sdk::mixnet::MixnetClient>>,
    received: &nym_sdk::mixnet::ReconstructedMessage,
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    
    let rows = sqlx::query_as::<_, (String, String, String, String, String)>(
        "SELECT address, version, providers, registered_at, expires_at FROM servers WHERE expires_at > ?"
    )
    .bind(&now)
    .fetch_all(&db)
    .await?;
    
    let servers: Vec<ServerEntry> = rows
        .into_iter()
        .filter_map(|(address, version, providers_json, registered_at, expires_at)| {
            let providers: Vec<Provider> = serde_json::from_str(&providers_json).ok()?;
            Some(ServerEntry {
                address,
                version,
                providers,
                registered_at,
                expires_at,
            })
        })
        .collect();
    
    println!("   Found {} servers", servers.len());
    
    let response = DirectoryResponse::ServerList { servers };
    let response_bytes = serde_json::to_vec(&response)?;
    
    let mut client = nym_client.lock().await;
    
    if let Some(sender_tag) = &received.sender_tag {
        client.send_reply(sender_tag.clone(), response_bytes).await?;
        println!("   ✅ Sent via SURB");
    } else if let Some(addr) = reply_to {
        let recipient = Recipient::try_from_base58_string(&addr)?;
        client.send_message(recipient, response_bytes, IncludedSurbs::none()).await?;
        println!("   ✅ Sent to client");
    }
    
    Ok(())
}

async fn handle_register(
    address: String,
    _version: String,
    providers: Vec<Provider>,
    reply_to: String,
    db: SqlitePool,
    nym_client: Arc<Mutex<nym_sdk::mixnet::MixnetClient>>,
    received: &nym_sdk::mixnet::ReconstructedMessage,
    pending_providers: Arc<Mutex<HashMap<String, Vec<Provider>>>>,
) -> Result<()> {
    let challenge_id = Uuid::new_v4().to_string();
    let number: u32 = rand::random();
    let created_at = Utc::now();
    let expires_at = created_at + Duration::minutes(5);
    
    // Store providers in memory for this challenge
    {
        let mut pending = pending_providers.lock().await;
        pending.insert(challenge_id.clone(), providers.clone());
    }
    
    sqlx::query(
        "INSERT INTO pending_challenges (challenge_id, address, number, created_at, expires_at) VALUES (?, ?, ?, ?, ?)"
    )
    .bind(&challenge_id)
    .bind(&address)
    .bind(number as i64)
    .bind(created_at.to_rfc3339())
    .bind(expires_at.to_rfc3339())
    .execute(&db)
    .await?;
    
    println!("   🎲 Challenge: {}", number);
    
    let challenge_response = DirectoryResponse::Challenge {
        challenge_id: challenge_id.clone(),
        number,
        expires_at: expires_at.to_rfc3339(),
    };
    
    let challenge_bytes = serde_json::to_vec(&challenge_response)?;
    let mut client = nym_client.lock().await;
    
    if let Some(sender_tag) = &received.sender_tag {
        client.send_reply(sender_tag.clone(), challenge_bytes).await?;
        println!("   📤 Challenge sent via SURB");
    } else if !reply_to.is_empty() {
        let recipient = Recipient::try_from_base58_string(&reply_to)?;
        client.send_message(recipient, challenge_bytes, IncludedSurbs::none()).await?;
        println!("   📤 Challenge sent to reply_to");
    }
    
    println!("   ⏳ Waiting for response...");
    
    Ok(())
}

async fn handle_challenge_response(
    challenge_id: String,
    response: u32,
    db: SqlitePool,
    nym_client: Arc<Mutex<nym_sdk::mixnet::MixnetClient>>,
    pending_providers: Arc<Mutex<HashMap<String, Vec<Provider>>>>,
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    
    let challenge_opt = sqlx::query_as::<_, (String, i64)>(
        "SELECT address, number FROM pending_challenges WHERE challenge_id = ? AND expires_at > ?"
    )
    .bind(&challenge_id)
    .bind(&now)
    .fetch_optional(&db)
    .await?;
    
    let (address, number) = match challenge_opt {
        Some(c) => c,
        None => {
            println!("   ❌ Challenge not found or expired");
            return Ok(());
        }
    };
    
    if number != response as i64 {
        println!("   ❌ Wrong response!");
        return Ok(());
    }
    
    println!("   ✅ Challenge solved!");
    
    // Get providers from memory
    let providers = {
        let mut pending = pending_providers.lock().await;
        pending.remove(&challenge_id).unwrap_or_else(|| vec![Provider::Groq])
    };
    
    let registered_at = Utc::now();
    let expires_at = registered_at + Duration::hours(24);
    
    let providers_json = serde_json::to_string(&providers)?;
    
    sqlx::query(
        "INSERT OR REPLACE INTO servers (address, version, providers, registered_at, expires_at) VALUES (?, ?, ?, ?, ?)"
    )
    .bind(&address)
    .bind("1.0.0")
    .bind(&providers_json)
    .bind(registered_at.to_rfc3339())
    .bind(expires_at.to_rfc3339())
    .execute(&db)
    .await?;
    
    sqlx::query("DELETE FROM pending_challenges WHERE challenge_id = ?")
        .bind(&challenge_id)
        .execute(&db)
        .await?;
    
    println!("\n╔══════════════════════════════════════════╗");
    println!("║     ✅ REGISTRATION SUCCESSFUL          ║");
    println!("╚══════════════════════════════════════════╝");
    println!("   Server: {}...", &address[..20]);
    println!("   Providers: {:?}", providers);
    
    let success_response = DirectoryResponse::RegistrationSuccess {
        expires_at: expires_at.to_rfc3339(),
    };
    let response_bytes = serde_json::to_vec(&success_response)?;
    
    let recipient = Recipient::try_from_base58_string(&address)?;
    let mut client = nym_client.lock().await;
    client.send_message(recipient, response_bytes, IncludedSurbs::none()).await?;
    
    println!("   📤 Confirmation sent");
    println!("═══════════════════════════════════════\n");
    
    Ok(())
}

async fn status_printer(db: SqlitePool) {
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
        
        let now = Utc::now().to_rfc3339();
        let servers = sqlx::query_as::<_, (String, String)>(
            "SELECT address, providers FROM servers WHERE expires_at > ?"
        )
        .bind(&now)
        .fetch_all(&db)
        .await
        .unwrap_or_default();
        
        if !servers.is_empty() {
            println!("\n📊 Active Servers: {}", servers.len());
            for (addr, providers_json) in &servers {
                let providers: Vec<Provider> = serde_json::from_str(providers_json).unwrap_or_default();
                println!("   {}... - APIs: {:?}", &addr[..20], providers);
            }
        }
    }
}

async fn cleanup_task(db: SqlitePool) {
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(3600)).await;
        let now = Utc::now().to_rfc3339();
        
        let _ = sqlx::query("DELETE FROM servers WHERE expires_at < ?")
            .bind(&now)
            .execute(&db)
            .await;
            
        let _ = sqlx::query("DELETE FROM pending_challenges WHERE expires_at < ?")
            .bind(&now)
            .execute(&db)
            .await;
    }
}
