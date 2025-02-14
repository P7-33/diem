// Copyright (c) The Diem Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{BlockingClient, Error, Result};
use diem_types::{
    account_address::AccountAddress,
    transaction::{authenticator::AuthenticationKey, SignedTransaction},
};
use reqwest::Url;

pub struct FaucetClient {
    url: String,
    json_rpc_client: BlockingClient,
}

impl FaucetClient {
    pub fn new(url: String, json_rpc_url: String) -> Self {
        Self {
            url,
            json_rpc_client: BlockingClient::new(json_rpc_url),
        }
    }

    pub fn create_account(&self, authentication_key: AuthenticationKey) -> Result<()> {
        let client = reqwest::blocking::Client::new();
        let mut url = Url::parse(&self.url).map_err(Error::request)?;
        url.set_path("accounts");
        let query = format!("authentication-key={}", authentication_key);
        url.set_query(Some(&query));

        // Faucet returns the transaction that creates the account and needs to be waited on before
        // returning.
        let response = client.post(url).send().map_err(Error::request)?;
        let status_code = response.status();
        let body = response.text().map_err(Error::decode)?;
        if !status_code.is_success() {
            return Err(Error::status(status_code.as_u16()));
        }
        let bytes = hex::decode(body).map_err(Error::decode)?;
        let txn: SignedTransaction = bcs::from_bytes(&bytes).map_err(Error::decode)?;

        self.json_rpc_client
            .wait_for_signed_transaction(&txn, None, None)
            .map_err(Error::unknown)?;

        Ok(())
    }

    pub fn fund(&self, address: AccountAddress, currency_code: &str, amount: u64) -> Result<()> {
        let client = reqwest::blocking::Client::new();
        let mut url = Url::parse(&self.url).map_err(Error::request)?;
        url.set_path(&format!("accounts/{}/fund", address));
        let query = format!("currency={}&amount={}", currency_code, amount);
        url.set_query(Some(&query));

        // Faucet returns the transaction that creates the account and needs to be waited on before
        // returning.
        let response = client.post(url).send().map_err(Error::request)?;
        let status_code = response.status();
        let body = response.text().map_err(Error::decode)?;
        if !status_code.is_success() {
            return Err(Error::status(status_code.as_u16()));
        }
        let bytes = hex::decode(body).map_err(Error::decode)?;
        let txn: SignedTransaction = bcs::from_bytes(&bytes).map_err(Error::decode)?;

        self.json_rpc_client
            .wait_for_signed_transaction(&txn, None, None)
            .map_err(Error::unknown)?;
        Ok(())
    }

    pub fn mint(
        &self,
        currency_code: &str,
        auth_key: AuthenticationKey,
        amount: u64,
    ) -> Result<()> {
        let client = reqwest::blocking::Client::new();

        let url = format!(
            "{}?amount={}&auth_key={}&currency_code={}&return_txns=true",
            self.url, amount, auth_key, currency_code,
        );

        // Faucet returns a list of txns that need to be waited on before returning. 1) a txn for
        // creating the account (if it doesn't already exist) and 2) to issue funds to said
        // account.
        let response = client.post(&url).send().map_err(Error::request)?;
        let status_code = response.status();
        let body = response.text().map_err(Error::decode)?;
        if !status_code.is_success() {
            return Err(Error::status(status_code.as_u16()));
        }
        let bytes = hex::decode(body).map_err(Error::decode)?;
        let txns: Vec<SignedTransaction> = bcs::from_bytes(&bytes).map_err(Error::decode)?;

        for txn in txns {
            self.json_rpc_client
                .wait_for_signed_transaction(&txn, None, None)
                .map_err(Error::unknown)?;
        }

        Ok(())
    }
}
