// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use futures::TryFutureExt;
use sn_transfers::{SignedSpend, Transfer};
use xor_name::XorName;

use super::Client;

use sn_protocol::NetworkAddress;
use sn_transfers::{CashNote, MainPubkey, NanoTokens};
use sn_transfers::{LocalWallet, OfflineTransfer, WalletError, WalletResult};

use futures::future::join_all;
use std::{
    collections::{BTreeMap, BTreeSet},
    iter::Iterator,
    time::{Duration, Instant},
};
use tokio::{task::JoinSet, time::sleep};

/// A wallet client can be used to send and
/// receive tokens to/from other wallets.
pub struct WalletClient {
    client: Client,
    wallet: LocalWallet,
}

impl WalletClient {
    /// Create a new wallet client.
    pub fn new(client: Client, wallet: LocalWallet) -> Self {
        Self { client, wallet }
    }

    /// Stores the wallet to disk.
    pub fn store_local_wallet(&self) -> WalletResult<()> {
        self.wallet.store()
    }

    /// Get the wallet balance
    pub fn balance(&self) -> NanoTokens {
        self.wallet.balance()
    }

    /// Do we have any unconfirmed transactions?
    pub fn unconfirmed_spend_requests_exist(&self) -> bool {
        self.wallet.unconfirmed_spend_requests_exist()
    }
    /// Get unconfirmed txs
    pub fn unconfirmed_spend_requests(&self) -> &BTreeSet<SignedSpend> {
        self.wallet.unconfirmed_spend_requests()
    }

    /// Get the payment transfers for a given network address
    pub fn get_payment_transfers(&self, address: &NetworkAddress) -> WalletResult<Vec<Transfer>> {
        match &address.as_xorname() {
            Some(xorname) => {
                let cash_notes = self.wallet.get_payment_cash_notes(xorname);
                Ok(Transfer::transfers_from_cash_notes(cash_notes)?)
            }
            None => Err(WalletError::InvalidAddressType),
        }
    }

    /// Send tokens to another wallet.
    /// Can optionally verify the store has been successful (this will attempt to GET the cash_note from the network)
    pub async fn send_cash_note(
        &mut self,
        amount: NanoTokens,
        to: MainPubkey,
        verify_store: bool,
    ) -> WalletResult<CashNote> {
        let created_cash_notes = self.wallet.local_send(vec![(amount, to)], None)?;

        // send to network
        if let Err(error) = self
            .client
            .send(self.wallet.unconfirmed_spend_requests(), verify_store)
            .await
        {
            return Err(WalletError::CouldNotSendMoney(format!(
                "The transfer was not successfully registered in the network: {error:?}"
            )));
        } else {
            // clear unconfirmed txs
            self.wallet.clear_unconfirmed_spend_requests();
        }

        // return the first CashNote (assuming there is only one because we only sent to one recipient)
        match &created_cash_notes[..] {
            [cashnote] => Ok(cashnote.clone()),
            [_multiple, ..] => Err(WalletError::CouldNotSendMoney(
                "Multiple CashNotes were returned from the transaction when only one was expected. This is a BUG."
                    .into(),
            )),
            [] => Err(WalletError::CouldNotSendMoney(
                "No CashNotes were returned from the wallet.".into(),
            )),
        }
    }

    /// Get storecost from the network
    /// Returns a Vec of proofs
    pub async fn get_store_cost_at_address(
        &self,
        address: &NetworkAddress,
    ) -> WalletResult<Vec<(MainPubkey, NanoTokens)>> {
        self.client
            .get_store_costs_at_address(address)
            .await
            .map_err(|error| WalletError::CouldNotSendMoney(error.to_string()))
    }

    /// Send tokens to nodes closest to the data we want to make storage payment for.
    ///
    /// Returns storage cost, storage cost is _per record_, and it's zero if not required for this operation.
    ///
    /// This can optionally verify the store has been successful (this will attempt to GET the cash_note from the network)
    pub async fn pay_for_storage(
        &mut self,
        content_addrs: impl Iterator<Item = NetworkAddress>,
        verify_store: bool,
    ) -> WalletResult<NanoTokens> {
        let mut total_cost = NanoTokens::zero();

        let mut payment_map = BTreeMap::default();

        let mut tasks = JoinSet::new();
        // we can collate all the payments together into one transfer
        for content_addr in content_addrs {
            let client = self.client.clone();
            tasks.spawn(async move {
                let costs = client
                    .get_store_costs_at_address(&content_addr)
                    .await
                    .map_err(|error| WalletError::CouldNotSendMoney(error.to_string()));

                debug!("Storecosts retrieved for {content_addr:?}");
                (content_addr, costs)
            });
        }

        debug!("Pending store cost tasks: {:?}", tasks.len());
        while let Some(res) = tasks.join_next().await {
            // In case of cann't fetch cost from network for a content,
            // just skip it as it will then get verification failure,
            // and repay/re-upload will be triggered correspondently.
            match res {
                Ok((content_addr, Ok(cost))) => {
                    if let Some(xorname) = content_addr.as_xorname() {
                        let _ = payment_map.insert(xorname, cost);
                        debug!("Storecosts inserted into payment map for {content_addr:?}");
                    } else {
                        warn!("Cannot get store cost for a content that is not a data type: {content_addr:?}");
                        println!("Cannot get store cost for a content that is not a data type: {content_addr:?}");
                    }
                }
                Ok((content_addr, Err(err))) => {
                    warn!("Cannot get store cost for {content_addr:?} with error {err:?}");
                    println!("Cannot get store cost for {content_addr:?} with error {err:?}");
                }
                Err(e) => {
                    warn!("Cannot get a store cost for a content with error {e:?}");
                    println!("Cannot get a store cost for a content with error {e:?}");
                }
            }
        }

        info!("Storecosts retrieved");

        if !payment_map.is_empty() {
            self.wallet.adjust_payment_map(&mut payment_map);

            let cost = self.pay_for_records(payment_map, verify_store).await?;

            if let Some(cost) = total_cost.checked_add(cost) {
                total_cost = cost;
            }
        }

        Ok(total_cost)
    }

    /// Send tokens to nodes closest to the data we want to make storage payment for.
    ///
    /// Returns the total amount paid.
    ///
    /// This can optionally verify the store has been successful (this will attempt to GET the cash_note from the network)
    pub async fn pay_for_records(
        &mut self,
        all_data_payments: BTreeMap<XorName, Vec<(MainPubkey, NanoTokens)>>,
        verify_store: bool,
    ) -> WalletResult<NanoTokens> {
        // TODO:
        // Check for any existing payment CashNotes, and use them if they exist, only topping up if needs be
        let num_of_payments = all_data_payments.len();

        let now = Instant::now();
        let mut total_cost = NanoTokens::zero();
        for (_data, costs) in all_data_payments.iter() {
            for (_target, cost) in costs {
                if let Some(new_cost) = total_cost.checked_add(*cost) {
                    total_cost = new_cost;
                } else {
                    return Err(WalletError::TotalPriceTooHigh);
                }
            }
        }

        self.wallet
            .local_send_storage_payment(all_data_payments, None)?;

        // send to network
        trace!("Sending storage payment transfer to the network");

        let spend_attempt_result = self
            .client
            .send(self.wallet.unconfirmed_spend_requests(), verify_store)
            .await;
        if let Err(error) = spend_attempt_result {
            warn!("The storage payment transfer was not successfully registered in the network: {error:?}. It will be retried later.");
            return Err(error);
        } else {
            info!("Spend has completed: {:?}", spend_attempt_result);
            self.wallet.clear_unconfirmed_spend_requests();
        }

        let elapsed = now.elapsed();
        println!("All transfers completed in {elapsed:?}");
        println!("Total payment: {total_cost:?} nano tokens for {num_of_payments:?} chunks");

        Ok(total_cost)
    }

    /// Resend failed txs
    /// This can optionally verify the store has been successful (this will attempt to GET the cash_note from the network)
    pub async fn resend_pending_txs(&mut self, verify_store: bool) {
        if self
            .client
            .send(self.wallet.unconfirmed_spend_requests(), verify_store)
            .await
            .is_ok()
        {
            self.wallet.clear_unconfirmed_spend_requests();
            // We might want to be _really_ sure and do the below
            // as well, but it's not necessary.
            // use crate::domain::wallet::VerifyingClient;
            // client.verify(tx_hash).await.ok();
        }
    }

    /// Return the wallet.
    pub fn into_wallet(self) -> LocalWallet {
        self.wallet
    }

    /// Return ref to inner waller
    pub fn mut_wallet(&mut self) -> &mut LocalWallet {
        &mut self.wallet
    }
}

impl Client {
    /// Send a spend request to the network.
    /// This can optionally verify the spend has been correctly stored before returning
    pub async fn send(
        &self,
        spend_requests: &BTreeSet<SignedSpend>,
        verify_store: bool,
    ) -> WalletResult<()> {
        let mut tasks = Vec::new();

        for spend_request in spend_requests {
            trace!(
                "sending spend request to the network: {:?}: {spend_request:#?}",
                spend_request.unique_pubkey()
            );
            tasks.push(self.network_store_spend(spend_request.clone(), verify_store));
        }

        for spend_attempt_result in join_all(tasks).await {
            spend_attempt_result.map_err(|err| WalletError::CouldNotSendMoney(err.to_string()))?;
        }

        Ok(())
    }

    /// Receive a Transfer, verify and redeem CashNotes from the Network.
    pub async fn receive(
        &self,
        transfer: Transfer,
        wallet: &LocalWallet,
    ) -> WalletResult<Vec<CashNote>> {
        let cashnotes = self
            .network
            .verify_and_unpack_transfer(transfer, wallet)
            .map_err(|e| WalletError::CouldNotReceiveMoney(format!("{e:?}")))
            .await?;
        Ok(cashnotes)
    }

    /// Send a spend request to the network.
    /// This does _not_ verify the spend has been put to the network correctly
    pub async fn send_without_verify(&self, transfer: OfflineTransfer) -> WalletResult<()> {
        let mut tasks = Vec::new();
        for spend_request in &transfer.all_spend_requests {
            trace!(
                "sending spend request to the network: {:?}: {spend_request:#?}",
                spend_request.unique_pubkey()
            );
            tasks.push(self.network_store_spend(spend_request.clone(), false));
        }

        for spend_attempt_result in join_all(tasks).await {
            spend_attempt_result.map_err(|err| WalletError::CouldNotSendMoney(err.to_string()))?;
        }

        Ok(())
    }

    pub async fn verify(&self, cash_note: &CashNote) -> WalletResult<()> {
        // We need to get all the spends in the cash_note from the network,
        // and compare them to the spends in the cash_note, to know if the
        // transfer is considered valid in the network.
        let mut tasks = Vec::new();
        for spend in &cash_note.signed_spends {
            tasks.push(self.get_spend_from_network(spend.unique_pubkey()));
        }

        let mut received_spends = std::collections::BTreeSet::new();
        for result in join_all(tasks).await {
            let network_valid_spend =
                result.map_err(|err| WalletError::CouldNotVerifyTransfer(err.to_string()))?;
            let _ = received_spends.insert(network_valid_spend);
        }

        // If all the spends in the cash_note are the same as the ones in the network,
        // we have successfully verified that the cash_note is globally recognised and therefor valid.
        if received_spends == cash_note.signed_spends {
            return Ok(());
        }
        Err(WalletError::CouldNotVerifyTransfer(
            "The spends in network were not the same as the ones in the CashNote. The parents of this CashNote are probably double spends.".into(),
        ))
    }
}

/// Use the client to send a CashNote from a local wallet to an address.
/// This marks the spent CashNote as spent in the Network
pub async fn send(
    from: LocalWallet,
    amount: NanoTokens,
    to: MainPubkey,
    client: &Client,
    verify_store: bool,
) -> WalletResult<CashNote> {
    if amount.as_nano() == 0 {
        panic!("Amount must be more than zero.");
    }

    let mut wallet_client = WalletClient::new(client.clone(), from);
    let new_cash_note = wallet_client
        .send_cash_note(amount, to, verify_store)
        .await
        .map_err(|err| {
            error!("Could not send cash note, err: {err:?}");
            err
        })?;

    let mut did_error = false;
    if verify_store {
        let mut attempts = 0;
        while wallet_client.unconfirmed_spend_requests_exist() {
            info!("Unconfirmed txs exist, sending again after 1 second...");
            sleep(Duration::from_secs(1)).await;
            wallet_client.resend_pending_txs(verify_store).await;

            if attempts > 10 {
                // save the error state, but break out of the loop so we can save
                did_error = true;
                break;
            }

            attempts += 1;
        }
    }

    let mut wallet = wallet_client.into_wallet();
    wallet.store()?;
    wallet.store_cash_note(&new_cash_note)?;

    if did_error {
        return Err(WalletError::UnconfirmedTxAfterRetries);
    }

    Ok(new_cash_note)
}