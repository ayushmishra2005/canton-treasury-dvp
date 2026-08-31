use anyhow::{anyhow, Context, Result};
use reqwest::blocking::Client;
use serde::Serialize;
use serde_json::Value;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct RelayerClient {
    http: Client,
    base_url: String,
    api_key: String,
    relayer_id: String,
}

#[derive(Serialize)]
struct InstructionSpec {
    program_id: String,
    accounts: Vec<AccountSpec>,
    data: String,
}

#[derive(Serialize)]
struct AccountSpec {
    pubkey: String,
    is_signer: bool,
    is_writable: bool,
}

#[derive(Clone, Debug)]
pub struct RelayerInstruction {
    pub program_id: String,
    pub accounts: Vec<(String, bool, bool)>,
    pub data: Vec<u8>,
}

impl RelayerClient {
    pub fn new(base_url: String, api_key: String, relayer_id: String) -> Result<Self> {
        Ok(Self {
            http: Client::builder().timeout(Duration::from_secs(30)).build()?,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            relayer_id,
        })
    }

    pub fn address(&self) -> Result<String> {
        let body = self.get(&format!("/api/v1/relayers/{}", self.relayer_id))?;
        body["data"]["address"]
            .as_str()
            .or_else(|| body["address"].as_str())
            .map(|value| value.to_string())
            .ok_or_else(|| anyhow!("relayer address missing from {body}"))
    }

    pub fn send_instructions(&self, instructions: &[RelayerInstruction]) -> Result<String> {
        let payload = serde_json::json!({
            "instructions": instructions.iter().map(|ix| InstructionSpec {
                program_id: ix.program_id.clone(),
                accounts: ix.accounts.iter().map(|(pubkey, signer, writable)| AccountSpec {
                    pubkey: pubkey.clone(),
                    is_signer: *signer,
                    is_writable: *writable,
                }).collect(),
                data: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &ix.data),
            }).collect::<Vec<_>>(),
        });
        let body = self.post(
            &format!("/api/v1/relayers/{}/transactions", self.relayer_id),
            payload,
        )?;
        self.extract_id(&body)
    }

    pub fn send_transaction(&self, serialized: &[u8]) -> Result<String> {
        let payload = serde_json::json!({
            "transaction": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, serialized),
        });
        let body = self.post(
            &format!("/api/v1/relayers/{}/transactions", self.relayer_id),
            payload,
        )?;
        self.extract_id(&body)
    }

    pub fn wait_confirmed(&self, id: &str, timeout: Duration) -> Result<String> {
        let started = Instant::now();
        loop {
            let body = self.get(&format!(
                "/api/v1/relayers/{}/transactions/{id}",
                self.relayer_id
            ))?;
            let status = body["data"]["status"]
                .as_str()
                .or_else(|| body["status"].as_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let hash = body["data"]["hash"]
                .as_str()
                .or_else(|| body["data"]["signature"].as_str())
                .or_else(|| body["hash"].as_str())
                .unwrap_or("")
                .to_string();
            if status.contains("confirm")
                || status.contains("mined")
                || status == "confirmed"
                || status == "success"
            {
                if hash.is_empty() {
                    return Err(anyhow!(
                        "relayer reported {status} for {id} without a signature"
                    ));
                }
                return Ok(hash);
            }
            if status.contains("fail") || status.contains("cancel") {
                return Err(anyhow!("relayer transaction {id} failed: {body}"));
            }
            if started.elapsed() > timeout {
                return Err(anyhow!(
                    "relayer transaction {id} not confirmed after {:?}: {body}",
                    timeout
                ));
            }
            std::thread::sleep(Duration::from_millis(400));
        }
    }

    fn extract_id(&self, body: &Value) -> Result<String> {
        body["data"]["id"]
            .as_str()
            .or_else(|| body["id"].as_str())
            .map(|value| value.to_string())
            .ok_or_else(|| anyhow!("relayer did not return a transaction id: {body}"))
    }

    fn get(&self, path: &str) -> Result<Value> {
        let response = self
            .http
            .get(format!("{}{path}", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .with_context(|| format!("GET {path}"))?;
        self.json(response, path)
    }

    fn post(&self, path: &str, payload: Value) -> Result<Value> {
        let response = self
            .http
            .post(format!("{}{path}", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&payload)
            .send()
            .with_context(|| format!("POST {path}"))?;
        self.json(response, path)
    }

    fn json(&self, response: reqwest::blocking::Response, path: &str) -> Result<Value> {
        let status = response.status();
        let body = response.text()?;
        if !status.is_success() {
            return Err(anyhow!("relayer {path} returned {status}: {body}"));
        }
        serde_json::from_str(&body).with_context(|| format!("relayer {path} json: {body}"))
    }
}
