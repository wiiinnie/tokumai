use anyhow::Result;
use dotenv::dotenv;
use nym_sdk::mixnet::{self, MixnetMessageSender, Recipient, IncludedSurbs};
use reqwest::Client;
use scramble_shared::{Request, Response, ChatRequest, ChatResponse, Provider};
use std::env;

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    
    println!("🚀 ScrambleAI Server");
    println!("====================\n");
    
    // Check API keys
    let groq_key = env::var("GROQ_API_KEY").ok();
    let openai_key = env::var("OPENAI_API_KEY").ok();
    let anthropic_key = env::var("ANTHROPIC_API_KEY").ok();
    
    println!("📋 Available Providers:");
    if groq_key.is_some() {
        println!("  ✅ Groq");
    }
    if openai_key.is_some() {
        println!("  ✅ OpenAI");
    }
    if anthropic_key.is_some() {
        println!("  ✅ Anthropic");
    }
    println!();
    
    // Initialize Nym client
    println!("🚀 Initializing Nym Server...");
    let client = mixnet::MixnetClientBuilder::new_ephemeral()
        .build()?;
    
    // Connect to mixnet
    let mut nym_client = client.connect_to_mixnet().await?;
    
    let server_address = nym_client.nym_address();
    println!("✅ Server Ready!");
    println!("📍 Server Nym Address:");
    println!("   {}", server_address);
    println!("\n⏳ Listening for requests (supports both SURBs and Fast mode)...\n");
    
    // HTTP client for API calls
    let http_client = Client::new();
    
    // Main event loop
    loop {
        // Wait for incoming messages
        if let Some(messages) = nym_client.wait_for_messages().await {
            for received in messages {
                println!("📨 Received request at {}", chrono::Local::now().format("%H:%M:%S"));
                
                // Parse request
                let request: Request = match serde_json::from_slice(&received.message) {
                    Ok(req) => req,
                    Err(e) => {
                        eprintln!("❌ Failed to parse request: {}", e);
                        continue;
                    }
                };
                
                // Determine reply mode
                let (use_surbs, reply_address) = match &request {
                    Request::Ping { reply_to, .. } => {
                        (reply_to.is_none(), reply_to.clone())
                    }
                    Request::Chat(chat_req) => {
                        (chat_req.reply_to.is_none(), chat_req.reply_to.clone())
                    }
                };
                
                if use_surbs {
                    println!("   🔒 Maximum Privacy Mode (using SURBs)");
                    let sender_tag = match &received.sender_tag {
                        Some(tag) => tag,
                        None => {
                            eprintln!("   ❌ No sender tag but client requested SURBs!");
                            continue;
                        }
                    };
                    
                    // Process and respond via SURB
                    let response = process_request(request, &http_client, groq_key.as_deref(), 
                        openai_key.as_deref(), anthropic_key.as_deref()).await;
                    
                    let response_bytes = serde_json::to_vec(&response)?;
                    println!("📤 Sending via SURB...");
                    
                    match nym_client.send_reply(sender_tag.clone(), response_bytes).await {
                        Ok(_) => println!("✅ Response sent\n"),
                        Err(e) => eprintln!("❌ Failed to send reply: {}\n", e),
                    }
                } else {
                    println!("   🚀 Fast Mode (direct reply)");
                    let reply_addr = match reply_address {
                        Some(addr) => addr,
                        None => {
                            eprintln!("   ❌ No reply address provided!");
                            continue;
                        }
                    };
                    
                    println!("   Reply to: {}", reply_addr);
                    
                    // Process request
                    let response = process_request(request, &http_client, groq_key.as_deref(),
                        openai_key.as_deref(), anthropic_key.as_deref()).await;
                    
                    // Send directly to client address
                    let recipient = match Recipient::try_from_base58_string(&reply_addr) {
                        Ok(r) => r,
                        Err(e) => {
                            eprintln!("   ❌ Invalid client address: {}", e);
                            continue;
                        }
                    };
                    
                    let response_bytes = serde_json::to_vec(&response)?;
                    println!("📤 Sending directly to client...");
                    
                    match nym_client.send_message(recipient, response_bytes, IncludedSurbs::none()).await {
                        Ok(_) => println!("✅ Response sent\n"),
                        Err(e) => eprintln!("❌ Failed to send: {}\n", e),
                    }
                }
            }
        }
    }
}

async fn process_request(
    request: Request,
    http_client: &Client,
    groq_key: Option<&str>,
    openai_key: Option<&str>,
    anthropic_key: Option<&str>,
) -> Response {
    match request {
        Request::Ping { timestamp, message, .. } => {
            println!("🏓 PING: {}", message);
            Response::pong(timestamp, &format!("pong: {}", message))
        }
        Request::Chat(chat_request) => {
            println!("💬 CHAT: {}", chat_request.prompt);
            
            match make_api_call(http_client, &chat_request, groq_key, openai_key, anthropic_key).await {
                Ok(response) => {
                    println!("✅ Generated {} chars", response.content.len());
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
        }
    }
}

async fn make_api_call(
    client: &Client,
    request: &ChatRequest,
    groq_key: Option<&str>,
    openai_key: Option<&str>,
    anthropic_key: Option<&str>,
) -> Result<ChatResponse> {
    match request.provider {
        Provider::Groq => {
            let api_key = groq_key.ok_or_else(|| anyhow::anyhow!("Groq API key not set"))?;
            call_groq(client, api_key, request).await
        }
        Provider::OpenAI => {
            let api_key = openai_key.ok_or_else(|| anyhow::anyhow!("OpenAI API key not set"))?;
            call_openai(client, api_key, request).await
        }
        Provider::Anthropic => {
            let api_key = anthropic_key.ok_or_else(|| anyhow::anyhow!("Anthropic API key not set"))?;
            call_anthropic(client, api_key, request).await
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

async fn call_openai(client: &Client, api_key: &str, request: &ChatRequest) -> Result<ChatResponse> {
    let body = serde_json::json!({
        "model": request.model,
        "messages": [{"role": "user", "content": request.prompt}]
    });
    
    let response = client
        .post("https://api.openai.com/v1/chat/completions")
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
        provider: Provider::OpenAI,
    })
}

async fn call_anthropic(client: &Client, api_key: &str, request: &ChatRequest) -> Result<ChatResponse> {
    let body = serde_json::json!({
        "model": request.model,
        "messages": [{"role": "user", "content": request.prompt}],
        "max_tokens": 1024
    });
    
    let response = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await?;
    
    let json: serde_json::Value = response.json().await?;
    
    let content = json["content"][0]["text"]
        .as_str()
        .unwrap_or("No response")
        .to_string();
    
    Ok(ChatResponse {
        content,
        model: request.model.clone(),
        provider: Provider::Anthropic,
    })
}
