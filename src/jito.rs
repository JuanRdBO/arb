#![allow(dead_code)]
use crate::transaction::build_and_sign_tx;
use anyhow::Result;
use base58::ToBase58;
use jsonrpsee::core::client::ClientT;
use jsonrpsee::http_client::HttpClient;
use jsonrpsee::rpc_params;
use serde::Deserialize;
use solana_program::native_token::LAMPORTS_PER_SOL;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{read_keypair_file, Keypair};
use solana_sdk::signer::Signer;
use solana_sdk::system_instruction;
use solana_sdk::transaction::VersionedTransaction;
use std::sync::Arc;

#[derive(Clone)]
pub struct JitoClient {
    pub rpc_client: Arc<RpcClient>,
    pub wss_client: Arc<std::sync::RwLock<u64>>,    
    pub jsonrpc_client: HttpClient,
    pub keypair_filepath: String,
}

impl JitoClient {
    pub fn new(
        rpc_client: Arc<RpcClient>,
        wss_client: Arc<std::sync::RwLock<u64>>,
        jsonrpc_client: HttpClient,
        keypair_filepath: String,
    ) -> Self {
        Self {
            rpc_client,
            keypair_filepath,
            wss_client,
            jsonrpc_client,
        }
    }

    pub fn signer(&self) -> Keypair {
        read_keypair_file(&self.keypair_filepath).expect("Failed to load keypair")
    }

    pub async fn send_bundle(&mut self, txs: &[VersionedTransaction]) -> Result<()> {
        let jito_tip = self.wss_client.read().unwrap();

        let tippers: Vec<String> = self
            .jsonrpc_client
            .request("getTipAccounts", rpc_params![""])
            .await?;

        println!(
            "Jito tip: {} SOL",
            (*jito_tip as f64) / (LAMPORTS_PER_SOL as f64)
        );
        let tip_ix = system_instruction::transfer(
            &self.signer().pubkey(),
            &Pubkey::try_from(tippers[0].to_string().as_str()).unwrap(),
            *jito_tip,
        );
        // print amount in sol not lamports
        println!("SOL (Jito) tip: {:?}", (*jito_tip as f64) / (LAMPORTS_PER_SOL as f64));
        let tip_tx = build_and_sign_tx(&self.rpc_client, &self.signer(), &[tip_ix]).await?;

        let txs: Vec<String> = [txs, &[tip_tx]]
            .concat()
            .iter()
            .map(|tx| bincode::serialize(tx).unwrap().to_base58())
            .collect::<Vec<String>>();

        println!("Submitting transaction...");
        let params = rpc_params![txs];
        let resp: Result<String, _> = self.jsonrpc_client.request("sendBundle", params).await;
        match resp {
            Ok(bundle) => {
                let now = chrono::Local::now();
                let bundle_url = format!("https://explorer.jito.wtf/bundle/{bundle}");
                println!(
                    "[{}] {}",
                    now.format("%Y-%m-%d %H:%M:%S"),
                    bundle_url
                );

                match self.check_bundle_status(&bundle).await {
                    Ok(BundleStatusEnum::Landed) => println!("Bundle landed successfully {}\n", bundle_url),
                    Ok(BundleStatusEnum::Failed) => println!("Bundle failed to land {}\n", bundle_url),
                    Ok(BundleStatusEnum::Invalid) => println!("Bundle invalid {}\n", bundle_url),
                    Ok(BundleStatusEnum::Pending) => println!("Bundle pending {}\n", bundle_url),
                    Ok(BundleStatusEnum::Unknown) => println!("Bundle unknown {}\n", bundle_url),
                    Ok(BundleStatusEnum::Timeout) => println!("Bundle timeout {}\n", bundle_url),
                    Err(e) => eprintln!("Error checking bundle status: {:?}", e),
                }
            }
            Err(err) => {
                eprintln!("Error: {:?}", err);
            }
        }
        Ok(())
    }

    async fn check_bundle_status(&self, bundle_id: &str) -> Result<BundleStatusEnum> {
        let start_time = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(30);

        while start_time.elapsed() < timeout {
            let params = rpc_params![[bundle_id]];
            let response: Option<BundleStatusResponse> = self
                .jsonrpc_client
                .request("getInflightBundleStatuses", params)
                .await?;
     
            println!("Bundle status0: {:?}", response);

            if let Some(resp) = response {
                println!("Bundle status1: {:?}", resp);
                if let Some(status) = resp.value.first() {
                    println!("Bundle status2: {}", status.status);
                    match status.status.as_str() {
                        "Landed" => return Ok(BundleStatusEnum::Landed),
                        "Failed" => return Ok(BundleStatusEnum::Failed),
                        "Pending" | "Invalid" => {
                            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                            println!("Bundle elapsed: {:?}, is timeout: {:?}", start_time.elapsed(), start_time.elapsed() >= timeout);
                            if start_time.elapsed() >= timeout {
                                return Ok(BundleStatusEnum::Timeout);
                            }
                            continue;
                        }
                        _ => {
                            eprintln!("Unknown status: {}", status.status);
                            return Ok(BundleStatusEnum::Unknown);
                        }
                    }
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        }

        Ok(BundleStatusEnum::Timeout)
    }

    pub async fn get_jito_tip(&self) -> Result<u64> {
        let client = reqwest::Client::new();
        if let Ok(response) = client
            .get("https://bundles.jito.wtf/api/v1/bundles/tip_floor")
            .send()
            .await
        {
            if let Ok(tips) = response.json::<Vec<Tip>>().await {
                for item in tips {
                    return Ok((item.ema_landed_tips_50th_percentile * (10_f64).powf(9.0)) as u64);
                }
            }
        }
        Err(anyhow::anyhow!("Failed to get jito tip"))
    }
}

#[derive(Debug, Deserialize)]
pub struct Tip {
    pub time: String,
    pub landed_tips_25th_percentile: f64,
    pub landed_tips_50th_percentile: f64,
    pub landed_tips_75th_percentile: f64,
    pub landed_tips_95th_percentile: f64,
    pub landed_tips_99th_percentile: f64,
    pub ema_landed_tips_50th_percentile: f64,
}

#[derive(Debug, Deserialize)]
struct BundleStatus {
    bundle_id: String,
    status: String,
    landed_slot: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct BundleStatusResponse {
    context: Context,
    value: Vec<BundleStatus>,
}

#[derive(Debug, Deserialize)]
struct Context {
    slot: u64,
}

enum BundleStatusEnum {
    Landed,
    Failed,
    Pending,
    Invalid,
    Unknown,
    Timeout,
}

