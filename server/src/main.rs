use anyhow::Result;
use dotenv::dotenv;
use nym_sdk::mixnet::{self, MixnetMessageSender, Recipient, IncludedSurbs};
use reqwest::Client;
use scramble_shared::directory::{DirectoryRequest, DirectoryResponse};
use scramble_shared::{Request, Response, ChatRequest, ChatResponse, Provider};
use std::env;
use std::io::{self, Write};
use std::time::Instant;
use tokio::time::{Duration, sleep};

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    
    println!("🚀 ScrambleAI Server");
    println!("====================\n");
    
    // Check available API keys
    let groq_key = env::var("GROQ_API_KEY").ok();
    let openai_key = env::var("OPENAI_API_KEY").ok();
    let anthropic_key = env::var("ANTHROPIC_API_KEY").ok();
    
    let mut providers = Vec::new();
    if groq_key.is_some() {
        providers.push(Provider::Groq);
    }
    if openai_key.is_some() {
        providers.push(Provider::OpenAI);
    }
    if anthropic_key.is_some() {
        providers.push(Provider::Anthropic);
    }
    
    if providers.is_empty() {
        eprintln!("❌ No API keys configured!");
        eprintln!("   Set at least one: GROQ_API_KEY, OPENAI_API_KEY, or ANTHROPIC_API_KEY");
        return Ok(());
    }
    
    println!("✅ Available providers: {:?}\n", providers);
    
    // Gateway selection
    print!("Custom Gateway ID? (press ENTER for auto): ");
    io::stdout().flush()?;
    
    let mut gateway_choice = String::new();
    io::stdin().read_line(&mut gateway_choice)?;
    let gateway_choice = gateway_choice.trim();
    
    let gateway_id = if gateway_choice.is_empty() {
        println!("✅ Using automatic gateway selection\n");
        None
    } else if gateway_choice.len() > 40 {
        println!("✅ Using custom gateway: {}\n", gateway_choice);
        Some(gateway_choice.to_string())
    } else {
        println!("⚠️  Gateway ID too short, using auto\n");
        None
    };
    
    // Directory address
    print!("Enter Directory Nym address: ");
    io::stdout().flush()?;
    let mut directory_address = String::new();
    io::stdin().read_line(&mut directory_address)?;
    let directory_address = directory_address.trim().to_string();
    
    if directory_address.is_empty() {
        eprintln!("❌ Directory address required!");
        return Ok(());
    }
    
    println!();
    
    // Registration mode
    println!("Select registration mode:");
    println!("1. 🚀 Fast Mode (directory sees your address, ~30s)");
    println!("2. 🔒 Privacy Mode (mixnet-blind registration, ~60s)");
    print!("\nChoice [1/2] (default: 2): ");
    io::stdout().flush()?;
    
    let mut mode_input = String::new();
    io::stdin().read_line(&mut mode_input)?;
    let use_surbs = mode_input.trim() != "1";
    
    if use_surbs {
        println!("✅ Privacy Mode selected");
        println!("   🔒 Mixnet cannot see your address");
        println!("   ⏱️  Expected time: 40-80 seconds\n");
    } else {
        println!("✅ Fast Mode selected");
        println!("   📍 Address visible to directory");
        println!("   ⏱️  Expected time: 20-40 seconds\n");
    }
    
    println!("🚀 Initializing Nym Server...");
    let init_start = Instant::now();
    
    let mut builder = mixnet::MixnetClientBuilder::new_ephemeral();
    
    if let Some(gw_id) = gateway_id {
        println!("   🎯 Requesting specific gateway...");
        builder = builder.request_gateway(gw_id);
    } else {
        println!("   🎲 Auto-selecting gateway...");
    }
    
    let client = builder.build()?;
    println!("   ⏱️  Client built: {:.2}s", init_start.elapsed().as_secs_f64());
    
    let mut nym_client = client.connect_to_mixnet().await?;
    println!("   ⏱️  Connected: {:.2}s", init_start.elapsed().as_secs_f64());
    
    let server_address = nym_client.nym_address().to_string();
    let gateway_str = nym_client.nym_address().gateway().to_string();
    
    println!("\n✅ Server Ready!");
    println!("📍 Server Nym Address:");
    println!("   {}", server_address);
    println!("\n🌐 Gateway Info:");
    println!("   Gateway ID: {}", gateway_str);
    println!("   Providers: {:?}", providers);
    println!("⏱️  Total init time: {:.2}s\n", init_start.elapsed().as_secs_f64());
    
    println!("╔══════════════════════════════════════════╗");
    println!("║  🔄 STARTING DIRECTORY REGISTRATION     ║");
    println!("╚══════════════════════════════════════════╝\n");
    
    let registration_start = Instant::now();
    
    match register_with_directory(&directory_address, &server_address, providers.clone(), use_surbs, &mut nym_client).await {
        Ok(_) => {
            println!("\n╔══════════════════════════════════════════╗");
            println!("║     ✅ REGISTRATION SUCCESSFUL          ║");
            println!("╚══════════════════════════════════════════╝");
            println!("⏱️  Total registration time: {:.2}s\n", registration_start.elapsed().as_secs_f64());
        }
        Err(e) => {
            eprintln!("\n╔══════════════════════════════════════════╗");
            eprintln!("║     ⚠️  REGISTRATION TIMEOUT            ║");
            eprintln!("╚══════════════════════════════════════════╝");
            eprintln!("Error: {}", e);
            eprintln!("⏱️  Timed out after: {:.2}s\n", registration_start.elapsed().as_secs_f64());
        }
    }
    
    println!("⏳ Listening for client requests...\n");
    
    let http_client = Client::new();
    
    loop {
        if let Some(messages) = nym_client.wait_for_messages().await {
            for received in messages {
                // Ignore directory responses
                if let Ok(_) = serde_json::from_slice::<DirectoryResponse>(&received.message) {
                    continue;
                }
                
                println!("📨 Received request at {}", chrono::Local::now().format("%H:%M:%S"));
                
                let request: Request = match serde_json::from_slice(&received.message) {
                    Ok(req) => req,
                    Err(e) => {
                        eprintln!("❌ Failed to parse: {}", e);
                        continue;
                    }
                };
                
                let (use_surbs, reply_address) = match &request {
                    Request::Ping { reply_to, .. } => (reply_to.is_none(), reply_to.clone()),
                    Request::Chat(chat_req) => (chat_req.reply_to.is_none(), chat_req.reply_to.clone()),
                };
                
                let response = process_request(request, &http_client, groq_key.as_deref()).await;
                let response_bytes = serde_json::to_vec(&response)?;
                
                if use_surbs {
                    if let Some(sender_tag) = &received.sender_tag {
                        nym_client.send_reply(sender_tag.clone(), response_bytes).await?;
                        println!("✅ Sent via SURB\n");
                    }
                } else if let Some(addr) = reply_address {
                    let recipient = Recipient::try_from_base58_string(&addr)?;
                    nym_client.send_message(recipient, response_bytes, IncludedSurbs::none()).await?;
                    println!("✅ Sent\n");
                }
            }
        }
    }
}

async fn register_with_directory(
    dir_address: &str,
    server_address: &str,
    providers: Vec<Provider>,
    use_surbs: bool,
    nym_client: &mut nym_sdk::mixnet::MixnetClient,
) -> Result<()> {
    let reg_start = Instant::now();
    let recipient = Recipient::try_from_base58_string(dir_address)?;
    
    let reply_to = if use_surbs {
        String::new()
    } else {
        server_address.to_string()
    };
    
    let request = DirectoryRequest::Register {
        address: server_address.to_string(),
        version: "1.0.0".to_string(),
        providers,
        reply_to,
    };
    
    let request_bytes = serde_json::to_vec(&request)?;
    
    let surbs = if use_surbs {
        println!("   🔒 Privacy Mode: Using SURBs for challenge");
        IncludedSurbs::Amount(5)
    } else {
        println!("   🚀 Fast Mode: Using reply_to");
        IncludedSurbs::none()
    };
    
    println!("\n📤 Step 1: Sending registration request...");
    let send_start = Instant::now();
    nym_client.send_message(recipient, request_bytes, surbs).await?;
    println!("   ✅ Sent in {:.2}ms", send_start.elapsed().as_millis());
    
    println!("\n⏳ Step 2: Waiting for challenge...");
    
    let timeout_duration = Duration::from_secs(120);
    let wait_start = Instant::now();
    let mut poll_count = 0;
    
    loop {
        if wait_start.elapsed() > timeout_duration {
            return Err(anyhow::anyhow!("Timeout waiting for challenge after {:.2}s", wait_start.elapsed().as_secs_f64()));
        }
        
        poll_count += 1;
        if poll_count % 20 == 0 {
            println!("   ⏱️  Still waiting... {:.1}s elapsed", wait_start.elapsed().as_secs_f64());
        }
        
        if let Some(messages) = nym_client.wait_for_messages().await {
            for msg in messages {
                if let Ok(DirectoryResponse::Challenge { challenge_id, number, .. }) = 
                    serde_json::from_slice::<DirectoryResponse>(&msg.message) 
                {
                    let challenge_time = wait_start.elapsed().as_secs_f64();
                    println!("\n🎯 Step 3: Challenge received!");
                    println!("   ⏱️  After: {:.2}s", challenge_time);
                    println!("   Number to echo: {}", number);
                    
                    println!("\n📤 Step 4: Sending challenge response...");
                    let response_msg = DirectoryRequest::ChallengeResponse {
                        challenge_id,
                        response: number,
                    };
                    
                    let response_bytes = serde_json::to_vec(&response_msg)?;
                    
                    let response_start = Instant::now();
                    nym_client.send_message(recipient, response_bytes, IncludedSurbs::none()).await?;
                    println!("   ✅ Sent in {:.2}ms", response_start.elapsed().as_millis());
                    
                    // Wait for confirmation
                    println!("\n⏳ Step 5: Waiting for confirmation...");
                    let confirm_start = Instant::now();
                    let confirm_timeout = Duration::from_secs(120);
                    let mut confirm_poll = 0;
                    
                    loop {
                        if confirm_start.elapsed() > confirm_timeout {
                            return Err(anyhow::anyhow!("Timeout waiting for confirmation after {:.2}s", confirm_start.elapsed().as_secs_f64()));
                        }
                        
                        confirm_poll += 1;
                        if confirm_poll % 30 == 0 {
                            println!("   ⏱️  Still waiting... {:.1}s", confirm_start.elapsed().as_secs_f64());
                        }
                        
                        if let Some(confirm_msgs) = nym_client.wait_for_messages().await {
                            for confirm_msg in confirm_msgs {
                                if let Ok(DirectoryResponse::RegistrationSuccess { expires_at }) = 
                                    serde_json::from_slice::<DirectoryResponse>(&confirm_msg.message)
                                {
                                    let confirm_time = confirm_start.elapsed().as_secs_f64();
                                    println!("\n✅ Step 6: Confirmation received!");
                                    println!("   ⏱️  Confirmation: {:.2}s", confirm_time);
                                    println!("   Expires: {}", expires_at);
                                    
                                    println!("\n📊 TIMING:");
                                    println!("   Request → Challenge:  {:.2}s", challenge_time);
                                    println!("   Response → Confirm:   {:.2}s", confirm_time);
                                    println!("   ─────────────────────────────");
                                    println!("   Total:                {:.2}s", reg_start.elapsed().as_secs_f64());
                                    
                                    return Ok(());
                                }
                            }
                        }
                        
                        sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        }
        
        sleep(Duration::from_millis(500)).await;
    }
}

async fn process_request(
    request: Request,
    http_client: &Client,
    groq_key: Option<&str>,
) -> Response {
    match request {
        Request::Ping { timestamp, message, .. } => {
            println!("🏓 PING: {}", message);
            Response::pong(timestamp, &format!("pong: {}", message))
        }
        Request::Chat(chat_request) => {
            println!("💬 CHAT: {}", chat_request.prompt);
            
            if let Some(api_key) = groq_key {
                match call_groq(http_client, api_key, &chat_request).await {
                    Ok(response) => {
                        println!("✅ {} chars", response.content.len());
                        Response::Chat(response)
                    }
                    Err(e) => {
                        eprintln!("❌ API error: {}", e);
                        Response::Chat(ChatResponse {
                            content: format!("Error: {}", e),
                            model: chat_request.model,
                            provider: chat_request.provider,
                        })
                    }
                }
            } else {
                Response::Chat(ChatResponse {
                    content: "No API key".to_string(),
                    model: chat_request.model,
                    provider: chat_request.provider,
                })
            }
        }
    }
}

async fn call_groq(client: &Client, api_key: &str, request: &ChatRequest) -> Result<ChatResponse> {
    let body = serde_json::json!({
        "model": request.model,
        "messages": [{"role": "user", "content": request.prompt}]
    });
    
    let response = client
        .post("https://api.groq.com/openai/v1/chat/completions")
        .header("Authorization", format!("Bearer {}", api_key))
        .json(&body)
        .send()
        .await?;
    
    let json: serde_json::Value = response.json().await?;
    
    let content = json["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("No response")
        .to_string();
    
    Ok(ChatResponse {
        content,
        model: request.model.clone(),
        provider: Provider::Groq,
    })
}
