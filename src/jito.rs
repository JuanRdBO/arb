#![allow(dead_code)]
use crate::transaction::build_and_sign_tx;
use anyhow::Result;
use base58::ToBase58;
use jsonrpsee::core::client::ClientT;
use jsonrpsee::http_client::HttpClient;
use jsonrpsee::rpc_params;
use serde::Deserialize;
use serde_json::Value;
use solana_program::native_token::LAMPORTS_PER_SOL;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{read_keypair_file, Keypair};
use solana_sdk::signer::Signer;
use solana_sdk::system_instruction;
use solana_sdk::transaction::VersionedTransaction;
use tokio::time::sleep;
use std::sync::Arc;
use std::time::Duration;
use anyhow::anyhow;

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


            let (_recent_blockhash, last_valid_block_hash) =
            self.rpc_client.get_latest_blockhash_with_commitment(
                CommitmentConfig::confirmed()
            ).await?;

        println!("Submitting transaction...");        
        match self.send_transactions_with_tip(txs, last_valid_block_hash).await {
            Ok(bundle_id) => {
                println!("Transaction sent successfully: {}", bundle_id);
                Ok(())
            }
            Err(e) => Err(anyhow!("Failed to send transaction: {:?}", e)),
        }
    }

    pub async fn send_transactions_with_tip(
        &self,
        serialized_transactions: Vec<String>,
        last_valid_block_height: u64
    ) -> anyhow::Result<String> {
        // Send the transaction as a Jito bundle
        let bundle_id: String = self.send_jito_bundle(serialized_transactions).await?;
        sleep(Duration::from_secs(3)).await;
        // Poll for confirmation status
        let timeout: Duration = Duration::from_secs(60);
        let interval: Duration = Duration::from_secs(5);
        let start: tokio::time::Instant = tokio::time::Instant::now();

        println!("Bundle submitted: {}", format!("https://explorer.jito.wtf/bundle/{bundle_id}"));

        while
            start.elapsed() < timeout ||
            self.rpc_client.get_block_height().await? <= last_valid_block_height
        {
            let bundle_statuses: BundleStatusResponse = self.get_jito_bundle_statuses(
                vec![bundle_id.clone()]
            ).await?;

            if !bundle_statuses.value.is_empty() {
                // println!("bundle_statuses: {:?}", bundle_statuses);
                if bundle_statuses.value[0].status == "Landed" {
                    println!("bundle landed: {:?}", bundle_statuses.value[0].bundle_id);
                    return Ok(bundle_statuses.value[0].bundle_id.clone());
                } else if bundle_statuses.value[0].status == "Failed" {
                    return Err(anyhow!("Bundle failed"));
                }
            }

            sleep(interval).await;
        }

        Err(anyhow!("Bundle failed to confirm within the timeout period"))
    }

    pub async fn send_jito_bundle(
        &self,
        serialized_transactions: Vec<String>
    ) -> anyhow::Result<String> {
        println!("send_jito_bundle serialized_transactions: {:?}", serialized_transactions);
        let response: Value = self.jsonrpc_client.request(
            "sendBundle",
            rpc_params![serialized_transactions]
        ).await?;

        // println!("send_jito_bundle response: {:?}", response);
        if let Some(error) = response.get("error") {
            return Err(anyhow!("Error sending bundles: {:?}", error));
        }

        Ok(response.as_str().unwrap_or_default().to_string())
    }

    pub async fn get_jito_bundle_statuses(
        &self,
        bundle_ids: Vec<String>
    ) -> anyhow::Result<BundleStatusResponse> {
        // println!("get_bundle_statuses bundle_ids: {:?}", bundle_ids);
        let response: Option<BundleStatusResponse> = self.jsonrpc_client.request(
            "getInflightBundleStatuses",
            vec![bundle_ids]
        ).await?;

        // println!("get_bundle_statuses response: {:?}", response);

        if let Some(resp) = response {
            if let Some(status) = resp.value.first() {
                match status.status.as_str() {
                    "Landed" => {
                        return Ok(resp);
                    }
                    "Failed" => {
                        return Ok(resp);
                    }
                    "Pending" | "Invalid" => {
                        println!("Bundle is pending or invalid...");
                        let errors: Vec<BundleErrorResponse> = self.get_jito_bundle_error(
                            resp.value[0].bundle_id.clone()
                        ).await?;
                        if errors.is_empty() {
                            return Ok(resp);
                        }
                        return Err(anyhow!("Bundle failed"));
                    }
                    _ => {
                        eprintln!("Unknown status: {}", status.status);
                        return Ok(resp);
                    }
                }
            }
        }

        Err(anyhow!("Unexpected response format"))
    }

    pub async fn get_jito_bundle_error(
        &self,
        bundle_id: String
    ) -> anyhow::Result<Vec<BundleErrorResponse>> {
        let error_url =
            format!("https://bundles.jito.wtf/api/v1/bundles/get_bundle_error/{}", bundle_id);

        let error_response: reqwest::Response = reqwest::get(&error_url).await?;
        let errors: Vec<BundleErrorResponse> =
            error_response.json::<Vec<BundleErrorResponse>>().await?;

        for error in &errors {
            println!("Bundle error: {} - {}", error.error, error.error_details);
        }
        Ok(errors)
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
pub struct BundleErrorResponse {
    #[serde(rename = "bundleId")]
    pub bundle_id: String,
    pub error: String,
    #[serde(rename = "errorDetails")]
    pub error_details: String,
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

