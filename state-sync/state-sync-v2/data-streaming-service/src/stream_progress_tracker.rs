// Copyright (c) The Diem Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    data_notification::{
        AccountsWithProofRequest, DataClientRequest,
        DataClientRequest::{
            AccountsWithProof, EpochEndingLedgerInfos, NumberOfAccounts,
            TransactionOutputsWithProof, TransactionsWithProof,
        },
        DataClientResponse, DataNotification, DataPayload, EpochEndingLedgerInfosRequest,
        NumberOfAccountsRequest, TransactionOutputsWithProofRequest, TransactionsWithProofRequest,
    },
    error::Error,
    streaming_client::{
        Epoch, GetAllAccountsRequest, GetAllEpochEndingLedgerInfosRequest, StreamRequest,
    },
};
use diem_data_client::{AdvertisedData, GlobalDataSummary, ResponsePayload};
use diem_id_generator::{IdGenerator, U64IdGenerator};
use diem_types::{ledger_info::LedgerInfoWithSignatures, transaction::Version};
use enum_dispatch::enum_dispatch;
use itertools::Itertools;
use std::{cmp, sync::Arc};

macro_rules! invalid_client_request {
    ($client_request:expr, $stream_tracker:expr) => {
        panic!(
            "Invalid client request {:?} found for the data stream tracker {:?}",
            $client_request, $stream_tracker
        )
    };
}

macro_rules! invalid_response_type {
    ($client_response:expr) => {
        panic!(
            "The client response is type mismatched: {:?}",
            $client_response
        )
    };
}

macro_rules! invalid_stream_request {
    ($stream_request:expr) => {
        panic!(
            "Invalid stream request found {:?}",
            format!("{:?}", $stream_request)
        )
    };
}

/// The interface offered by each stream tracker.
#[enum_dispatch]
pub trait DataStreamTracker {
    /// Creates a batch of data client requests (up to `max_number_of_requests`)
    /// that can be sent to the diem data client to progress the stream.
    fn create_data_client_requests(
        &mut self,
        max_number_of_requests: u64,
        global_data_summary: &GlobalDataSummary,
    ) -> Result<Vec<DataClientRequest>, Error>;

    /// Returns true iff all remaining data required to satisfy the stream is
    /// available in the given advertised data.
    fn is_remaining_data_available(&self, advertised_data: &AdvertisedData) -> bool;

    /// Transforms a given data client response (for the previously sent
    /// request) into a data notification to be sent along the data stream.
    /// Note: this call may return `None`, in which case, no notification needs
    /// to be sent to the client.
    fn transform_client_response_into_notification(
        &mut self,
        client_request: &DataClientRequest,
        client_response: &DataClientResponse,
        notification_id_generator: Arc<U64IdGenerator>,
    ) -> Result<Option<DataNotification>, Error>;
}

/// A single progress tracker that allows each data stream type to track and
/// update progress through the `DataStreamTracker` interface.
#[enum_dispatch(DataStreamTracker)]
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum StreamProgressTracker {
    AccountsStreamTracker,
    ContinuousTransactionStreamTracker,
    EpochEndingStreamTracker,
    TransactionStreamTracker,
}

impl StreamProgressTracker {
    pub fn new(
        stream_request: &StreamRequest,
        advertised_data: &AdvertisedData,
    ) -> Result<Self, Error> {
        match stream_request {
            StreamRequest::ContinuouslyStreamTransactionOutputs(_) => {
                Ok(ContinuousTransactionStreamTracker::new(stream_request)?.into())
            }
            StreamRequest::ContinuouslyStreamTransactions(_) => {
                Ok(ContinuousTransactionStreamTracker::new(stream_request)?.into())
            }
            StreamRequest::GetAllAccounts(request) => {
                Ok(AccountsStreamTracker::new(request)?.into())
            }
            StreamRequest::GetAllEpochEndingLedgerInfos(request) => {
                Ok(EpochEndingStreamTracker::new(request, advertised_data)?.into())
            }
            StreamRequest::GetAllTransactionOutputs(_) => {
                Ok(TransactionStreamTracker::new(stream_request)?.into())
            }
            StreamRequest::GetAllTransactions(_) => {
                Ok(TransactionStreamTracker::new(stream_request)?.into())
            }
            _ => Err(Error::UnsupportedRequestEncountered(format!(
                "Stream request not currently supported: {:?}",
                stream_request
            ))),
        }
    }
}

#[derive(Clone, Debug)]
pub struct AccountsStreamTracker {
    // The original accounts request made by the client
    pub request: GetAllAccountsRequest,

    // True iff a request has been created to fetch the number of accounts
    pub account_num_requested: bool,

    // The total number of accounts to fetch at this version
    pub number_of_accounts: Option<u64>,

    // The next account index that we're waiting to send to the client along the
    // stream. All accounts before this index have already been sent.
    pub next_stream_index: u64,

    // The next account index that we're waiting to request from the network.
    // All accounts before this index have already been requested.
    pub next_request_index: u64,
}

impl AccountsStreamTracker {
    fn new(request: &GetAllAccountsRequest) -> Result<Self, Error> {
        Ok(AccountsStreamTracker {
            request: request.clone(),
            account_num_requested: false,
            number_of_accounts: None,
            next_stream_index: 0,
            next_request_index: 0,
        })
    }

    fn update_request_tracking(
        &mut self,
        client_requests: &[DataClientRequest],
    ) -> Result<(), Error> {
        for client_request in client_requests {
            match client_request {
                AccountsWithProof(request) => {
                    self.next_request_index =
                        request.end_index.checked_add(1).ok_or_else(|| {
                            Error::IntegerOverflow("Next request index has overflown!".into())
                        })?;
                }
                request => {
                    invalid_client_request!(request, self);
                }
            }
        }

        Ok(())
    }
}

impl DataStreamTracker for AccountsStreamTracker {
    fn create_data_client_requests(
        &mut self,
        max_number_of_requests: u64,
        global_data_summary: &GlobalDataSummary,
    ) -> Result<Vec<DataClientRequest>, Error> {
        if self.number_of_accounts.is_none() && self.account_num_requested {
            return Ok(vec![]); // Wait for the number of accounts to be returned
        }

        if let Some(number_of_accounts) = self.number_of_accounts {
            // Create the client requests
            let client_requests = create_data_client_requests(
                self.next_request_index,
                number_of_accounts,
                max_number_of_requests,
                global_data_summary
                    .optimal_chunk_sizes
                    .account_states_chunk_size,
                self.clone().into(),
            )?;
            self.update_request_tracking(&client_requests)?;

            Ok(client_requests)
        } else {
            self.account_num_requested = true;
            Ok(vec![DataClientRequest::NumberOfAccounts(
                NumberOfAccountsRequest {
                    version: self.request.version,
                },
            )])
        }
    }

    fn is_remaining_data_available(&self, advertised_data: &AdvertisedData) -> bool {
        AdvertisedData::contains_range(
            self.request.version,
            self.request.version,
            &advertised_data.account_states,
        )
    }

    fn transform_client_response_into_notification(
        &mut self,
        client_request: &DataClientRequest,
        client_response: &DataClientResponse,
        notification_id_generator: Arc<U64IdGenerator>,
    ) -> Result<Option<DataNotification>, Error> {
        match client_request {
            AccountsWithProof(request) => {
                verify_client_request_indices(
                    self.next_stream_index,
                    request.start_index,
                    request.end_index,
                );

                // Update the local stream notification tracker
                self.next_stream_index = request.end_index.checked_add(1).ok_or_else(|| {
                    Error::IntegerOverflow("Next stream index has overflown!".into())
                })?;

                // Create a new data notification
                let data_notification = create_data_notification(
                    notification_id_generator,
                    client_response,
                    self.clone().into(),
                );
                return Ok(Some(data_notification));
            }
            NumberOfAccounts(_) => {
                if let ResponsePayload::NumberOfAccountStates(number_of_accounts) =
                    client_response.payload
                {
                    // We got a response. Save the number of accounts.
                    self.number_of_accounts = Some(number_of_accounts);
                    self.account_num_requested = false;
                }
            }
            request => invalid_client_request!(request, self),
        }
        Ok(None)
    }
}

#[derive(Clone, Debug)]
pub struct ContinuousTransactionStreamTracker {
    // The original stream request made by the client (i.e., a continuous
    // transaction or transaction output stream request).
    pub request: StreamRequest,

    // The target ledger info that we're currently syncing to
    pub target_ledger_info: Option<LedgerInfoWithSignatures>,

    // True iff a request has been created to fetch an epoch ending ledger info
    pub end_of_epoch_requested: bool,

    // The next version and epoch that we're waiting to send to the
    // client along the stream. All versions before this have been sent.
    pub next_stream_version_and_epoch: (Version, Epoch),

    // The next version and epoch that we're waiting to request from
    // the network. All versions before this have been requested.
    pub next_request_version_and_epoch: (Version, Epoch),
}

impl ContinuousTransactionStreamTracker {
    fn new(stream_request: &StreamRequest) -> Result<Self, Error> {
        match stream_request {
            StreamRequest::ContinuouslyStreamTransactions(request) => {
                Ok(ContinuousTransactionStreamTracker {
                    request: stream_request.clone(),
                    target_ledger_info: None,
                    end_of_epoch_requested: false,
                    next_stream_version_and_epoch: (request.start_version, request.start_epoch),
                    next_request_version_and_epoch: (request.start_version, request.start_epoch),
                })
            }
            StreamRequest::ContinuouslyStreamTransactionOutputs(request) => {
                Ok(ContinuousTransactionStreamTracker {
                    request: stream_request.clone(),
                    target_ledger_info: None,
                    end_of_epoch_requested: false,
                    next_stream_version_and_epoch: (request.start_version, request.start_epoch),
                    next_request_version_and_epoch: (request.start_version, request.start_epoch),
                })
            }
            request => invalid_stream_request!(request),
        }
    }

    fn select_target_ledger_info(
        &self,
        advertised_data: &AdvertisedData,
    ) -> Result<LedgerInfoWithSignatures, Error> {
        if let Some(highest_synced_ledger_info) = highest_synced_ledger_info(advertised_data) {
            let (next_request_version, _) = self.next_request_version_and_epoch;
            if next_request_version > highest_synced_ledger_info.ledger_info().version() {
                Err(Error::NoDataToFetch(
                    "We're already at the highest synced ledger info version!".into(),
                ))
            } else {
                Ok(highest_synced_ledger_info)
            }
        } else {
            Err(Error::DataIsUnavailable(
                "Unable to find the highest synced ledger info!".into(),
            ))
        }
    }

    fn get_target_ledger_info(&self) -> &LedgerInfoWithSignatures {
        self.target_ledger_info
            .as_ref()
            .expect("No target ledger info found!")
    }

    fn create_data_notification(
        &mut self,
        request_end_version: Version,
        client_response: &DataClientResponse,
        notification_id_generator: Arc<U64IdGenerator>,
    ) -> Result<DataNotification, Error> {
        // Create a new data notification
        let data_notification = create_data_notification(
            notification_id_generator,
            client_response,
            self.clone().into(),
        );

        // Update the target ledger info if we've hit it
        if request_end_version == self.get_target_ledger_info().ledger_info().version() {
            self.target_ledger_info = None;
        }

        Ok(data_notification)
    }

    fn update_stream_version_and_epoch(
        &mut self,
        request_start_version: Version,
        request_end_version: Version,
    ) -> Result<(), Error> {
        let (next_stream_version, mut next_stream_epoch) = self.next_stream_version_and_epoch;
        verify_client_request_indices(
            next_stream_version,
            request_start_version,
            request_end_version,
        );

        // Update the next stream version and epoch
        if request_end_version == self.get_target_ledger_info().ledger_info().version()
            && self.get_target_ledger_info().ledger_info().ends_epoch()
        {
            next_stream_epoch = next_stream_epoch
                .checked_add(1)
                .ok_or_else(|| Error::IntegerOverflow("Next stream epoch has overflown!".into()))?;
        }
        let next_stream_version = request_end_version
            .checked_add(1)
            .ok_or_else(|| Error::IntegerOverflow("Next stream version has overflown!".into()))?;
        self.next_stream_version_and_epoch = (next_stream_version, next_stream_epoch);

        Ok(())
    }

    fn update_request_version_and_epoch(
        &mut self,
        request_end_version: Version,
    ) -> Result<(), Error> {
        let (_, mut next_request_epoch) = self.next_request_version_and_epoch;

        // Update the next request version and epoch
        if request_end_version == self.get_target_ledger_info().ledger_info().version()
            && self.get_target_ledger_info().ledger_info().ends_epoch()
        {
            // We've hit an epoch change
            next_request_epoch = next_request_epoch.checked_add(1).ok_or_else(|| {
                Error::IntegerOverflow("Next request epoch has overflown!".into())
            })?;
        }
        let next_request_version = request_end_version
            .checked_add(1)
            .ok_or_else(|| Error::IntegerOverflow("Next request version has overflown!".into()))?;
        self.next_request_version_and_epoch = (next_request_version, next_request_epoch);

        Ok(())
    }

    fn update_request_tracking(
        &mut self,
        client_requests: &[DataClientRequest],
    ) -> Result<(), Error> {
        match &self.request {
            StreamRequest::ContinuouslyStreamTransactions(_) => {
                for client_request in client_requests {
                    match client_request {
                        DataClientRequest::TransactionsWithProof(request) => {
                            self.update_request_version_and_epoch(request.end_version)?;
                        }
                        request => invalid_client_request!(request, self),
                    }
                }
            }
            StreamRequest::ContinuouslyStreamTransactionOutputs(_) => {
                for client_request in client_requests {
                    match client_request {
                        DataClientRequest::TransactionOutputsWithProof(request) => {
                            self.update_request_version_and_epoch(request.end_version)?;
                        }
                        request => invalid_client_request!(request, self),
                    }
                }
            }
            request => invalid_stream_request!(request),
        }

        Ok(())
    }
}

impl DataStreamTracker for ContinuousTransactionStreamTracker {
    fn create_data_client_requests(
        &mut self,
        max_number_of_requests: u64,
        global_data_summary: &GlobalDataSummary,
    ) -> Result<Vec<DataClientRequest>, Error> {
        if self.target_ledger_info.is_none() && self.end_of_epoch_requested {
            return Ok(vec![]); // We are waiting for the epoch ending ledger info
        }

        // If we don't have a syncing target, select one.
        let (next_request_version, next_request_epoch) = self.next_request_version_and_epoch;
        if self.target_ledger_info.is_none() {
            // Select a new ledger info from the advertised data
            let target_ledger_info =
                self.select_target_ledger_info(&global_data_summary.advertised_data)?;
            if target_ledger_info.ledger_info().epoch() > next_request_epoch {
                // There was an epoch change. Request an epoch ending ledger info.
                self.end_of_epoch_requested = true;
                return Ok(vec![DataClientRequest::EpochEndingLedgerInfos(
                    EpochEndingLedgerInfosRequest {
                        start_epoch: next_request_epoch,
                        end_epoch: next_request_epoch,
                    },
                )]);
            } else {
                self.target_ledger_info = Some(target_ledger_info);
            }
        }

        // We have a target ledger info.
        let target_ledger_info_version = self.get_target_ledger_info().ledger_info().version();
        if next_request_version > target_ledger_info_version {
            return Ok(vec![]); // Wait until all target notifications have been sent.
        }

        // Create the client requests
        let optimal_chunk_sizes = match &self.request {
            StreamRequest::ContinuouslyStreamTransactions(_) => {
                global_data_summary
                    .optimal_chunk_sizes
                    .transaction_chunk_size
            }
            StreamRequest::ContinuouslyStreamTransactionOutputs(_) => {
                global_data_summary
                    .optimal_chunk_sizes
                    .transaction_output_chunk_size
            }
            request => invalid_stream_request!(request),
        };
        let client_requests = create_data_client_requests(
            next_request_version,
            target_ledger_info_version,
            max_number_of_requests,
            optimal_chunk_sizes,
            self.clone().into(),
        )?;
        self.update_request_tracking(&client_requests)?;

        Ok(client_requests)
    }

    fn is_remaining_data_available(&self, advertised_data: &AdvertisedData) -> bool {
        let advertised_ranges = match &self.request {
            StreamRequest::ContinuouslyStreamTransactions(_) => &advertised_data.transactions,
            StreamRequest::ContinuouslyStreamTransactionOutputs(_) => {
                &advertised_data.transaction_outputs
            }
            request => invalid_stream_request!(request),
        };

        // Verify we can satisfy the next transaction version
        let (next_request_version, _) = self.next_request_version_and_epoch;
        AdvertisedData::contains_range(
            next_request_version,
            next_request_version,
            advertised_ranges,
        )
    }

    fn transform_client_response_into_notification(
        &mut self,
        client_request: &DataClientRequest,
        client_response: &DataClientResponse,
        notification_id_generator: Arc<U64IdGenerator>,
    ) -> Result<Option<DataNotification>, Error> {
        match client_request {
            EpochEndingLedgerInfos(_) => {
                if let ResponsePayload::EpochEndingLedgerInfos(epoch_ending_ledger_infos) =
                    &client_response.payload
                {
                    if let [target_ledger_info] = &epoch_ending_ledger_infos[..] {
                        self.target_ledger_info = Some(target_ledger_info.clone());
                    } else {
                        // TODO(joshlind): notify the data client of the bad response
                    }
                } else {
                    // TODO(joshlind): notify the data client of the bad response
                }
                self.end_of_epoch_requested = false;
                Ok(None)
            }
            TransactionsWithProof(request) => match &self.request {
                StreamRequest::ContinuouslyStreamTransactions(_) => {
                    self.update_stream_version_and_epoch(
                        request.start_version,
                        request.end_version,
                    )?;
                    let data_notification = self.create_data_notification(
                        request.end_version,
                        client_response,
                        notification_id_generator,
                    )?;
                    Ok(Some(data_notification))
                }
                request => invalid_stream_request!(request),
            },
            TransactionOutputsWithProof(request) => match &self.request {
                StreamRequest::ContinuouslyStreamTransactionOutputs(_) => {
                    self.update_stream_version_and_epoch(
                        request.start_version,
                        request.end_version,
                    )?;
                    let data_notification = self.create_data_notification(
                        request.end_version,
                        client_response,
                        notification_id_generator,
                    )?;
                    Ok(Some(data_notification))
                }
                request => invalid_stream_request!(request),
            },
            request => invalid_client_request!(request, self),
        }
    }
}

#[derive(Clone, Debug)]
pub struct EpochEndingStreamTracker {
    // The original epoch ending ledger infos request made by the client
    pub request: GetAllEpochEndingLedgerInfosRequest,

    // The last epoch ending ledger info that this stream will send to the client
    pub end_epoch: Epoch,

    // The next epoch that we're waiting to send to the client along the
    // stream. All epochs before this have already been sent.
    pub next_stream_epoch: Epoch,

    // The next epoch that we're waiting to request from the network. All epochs
    // before this have already been requested.
    pub next_request_epoch: Epoch,
}

impl EpochEndingStreamTracker {
    fn new(
        request: &GetAllEpochEndingLedgerInfosRequest,
        advertised_data: &AdvertisedData,
    ) -> Result<Self, Error> {
        let end_epoch = match most_common_highest_epoch(advertised_data) {
            Some(max_advertised_epoch) => {
                if max_advertised_epoch == 0 {
                    return Err(Error::NoDataToFetch(
                        "The maximum advertised epoch is 0. No epoch changes have occurred!".into(),
                    ));
                } else {
                    max_advertised_epoch.checked_sub(1).ok_or_else(|| {
                        Error::IntegerOverflow("Maximum advertised epoch has underflow!".into())
                    })?
                }
            }
            None => {
                return Err(Error::DataIsUnavailable(format!(
                    "Unable to find any maximum advertised epoch in the network: {:?}",
                    advertised_data
                )));
            }
        };

        if end_epoch < request.start_epoch {
            return Err(Error::DataIsUnavailable(format!(
                "The epoch to start syncing from is higher than any advertised highest epoch! Highest: {:?}, start: {:?}",
                end_epoch, request.start_epoch
            )));
        }

        Ok(EpochEndingStreamTracker {
            request: request.clone(),
            end_epoch,
            next_stream_epoch: request.start_epoch,
            next_request_epoch: request.start_epoch,
        })
    }

    fn update_request_tracking(
        &mut self,
        client_requests: &[DataClientRequest],
    ) -> Result<(), Error> {
        for client_request in client_requests {
            match client_request {
                EpochEndingLedgerInfos(request) => {
                    self.next_request_epoch =
                        request.end_epoch.checked_add(1).ok_or_else(|| {
                            Error::IntegerOverflow("Next request epoch has overflown!".into())
                        })?;
                }
                request => invalid_client_request!(request, self),
            }
        }

        Ok(())
    }
}

impl DataStreamTracker for EpochEndingStreamTracker {
    fn create_data_client_requests(
        &mut self,
        max_number_of_requests: u64,
        global_data_summary: &GlobalDataSummary,
    ) -> Result<Vec<DataClientRequest>, Error> {
        // Create the client requests
        let client_requests = create_data_client_requests(
            self.next_request_epoch,
            self.end_epoch,
            max_number_of_requests,
            global_data_summary.optimal_chunk_sizes.epoch_chunk_size,
            self.clone().into(),
        )?;
        self.update_request_tracking(&client_requests)?;

        Ok(client_requests)
    }

    fn is_remaining_data_available(&self, advertised_data: &AdvertisedData) -> bool {
        let start_epoch = self.next_stream_epoch;
        let end_epoch = self.end_epoch;
        AdvertisedData::contains_range(
            start_epoch,
            end_epoch,
            &advertised_data.epoch_ending_ledger_infos,
        )
    }

    fn transform_client_response_into_notification(
        &mut self,
        client_request: &DataClientRequest,
        client_response: &DataClientResponse,
        notification_id_generator: Arc<U64IdGenerator>,
    ) -> Result<Option<DataNotification>, Error> {
        match client_request {
            EpochEndingLedgerInfos(request) => {
                verify_client_request_indices(
                    self.next_stream_epoch,
                    request.start_epoch,
                    request.end_epoch,
                );

                // Update the local stream notification tracker
                self.next_stream_epoch = request.end_epoch.checked_add(1).ok_or_else(|| {
                    Error::IntegerOverflow("Next stream epoch has overflown!".into())
                })?;

                // Create a new data notification
                let data_notification = create_data_notification(
                    notification_id_generator,
                    client_response,
                    self.clone().into(),
                );
                Ok(Some(data_notification))
            }
            request => invalid_client_request!(request, self),
        }
    }
}

#[derive(Clone, Debug)]
pub struct TransactionStreamTracker {
    // The original stream request made by the client (i.e., a transaction or
    // transaction output stream request).
    pub request: StreamRequest,

    // The next version that we're waiting to send to the client
    // along the stream. All versions before this have been sent.
    pub next_stream_version: Version,

    // The next version that we're waiting to request from the
    // network. All versions before this have already been requested.
    pub next_request_version: Version,
}

impl TransactionStreamTracker {
    fn new(stream_request: &StreamRequest) -> Result<Self, Error> {
        match stream_request {
            StreamRequest::GetAllTransactions(request) => Ok(TransactionStreamTracker {
                request: stream_request.clone(),
                next_stream_version: request.start_version,
                next_request_version: request.start_version,
            }),
            StreamRequest::GetAllTransactionOutputs(request) => Ok(TransactionStreamTracker {
                request: stream_request.clone(),
                next_stream_version: request.start_version,
                next_request_version: request.start_version,
            }),
            request => invalid_stream_request!(request),
        }
    }

    fn update_stream_version(
        &mut self,
        request_start_version: Version,
        request_end_version: Version,
    ) -> Result<(), Error> {
        verify_client_request_indices(
            self.next_stream_version,
            request_start_version,
            request_end_version,
        );
        // Update the local stream notification tracker
        self.next_stream_version = request_end_version
            .checked_add(1)
            .ok_or_else(|| Error::IntegerOverflow("Next stream version has overflown!".into()))?;
        Ok(())
    }

    fn update_request_version(&mut self, request_end_version: Version) -> Result<(), Error> {
        self.next_request_version = request_end_version
            .checked_add(1)
            .ok_or_else(|| Error::IntegerOverflow("Next request version has overflown!".into()))?;
        Ok(())
    }

    fn update_request_tracking(
        &mut self,
        client_requests: &[DataClientRequest],
    ) -> Result<(), Error> {
        match &self.request {
            StreamRequest::GetAllTransactions(_) => {
                for client_request in client_requests.iter() {
                    match client_request {
                        TransactionsWithProof(request) => {
                            self.update_request_version(request.end_version)?;
                        }
                        request => invalid_client_request!(request, self),
                    }
                }
            }
            StreamRequest::GetAllTransactionOutputs(_) => {
                for client_request in client_requests.iter() {
                    match client_request {
                        TransactionOutputsWithProof(request) => {
                            self.update_request_version(request.end_version)?;
                        }
                        request => invalid_client_request!(request, self),
                    }
                }
            }
            request => invalid_stream_request!(request),
        }

        Ok(())
    }
}

impl DataStreamTracker for TransactionStreamTracker {
    fn create_data_client_requests(
        &mut self,
        max_number_of_requests: u64,
        global_data_summary: &GlobalDataSummary,
    ) -> Result<Vec<DataClientRequest>, Error> {
        let (request_end_version, optimal_chunk_sizes) = match &self.request {
            StreamRequest::GetAllTransactions(request) => (
                request.end_version,
                global_data_summary
                    .optimal_chunk_sizes
                    .transaction_chunk_size,
            ),
            StreamRequest::GetAllTransactionOutputs(request) => (
                request.end_version,
                global_data_summary
                    .optimal_chunk_sizes
                    .transaction_output_chunk_size,
            ),
            request => invalid_stream_request!(request),
        };

        // Create the client requests
        let client_requests = create_data_client_requests(
            self.next_request_version,
            request_end_version,
            max_number_of_requests,
            optimal_chunk_sizes,
            self.clone().into(),
        )?;
        self.update_request_tracking(&client_requests)?;

        Ok(client_requests)
    }

    fn is_remaining_data_available(&self, advertised_data: &AdvertisedData) -> bool {
        let (request_end_version, advertised_ranges) = match &self.request {
            StreamRequest::GetAllTransactions(request) => {
                (request.end_version, &advertised_data.transactions)
            }
            StreamRequest::GetAllTransactionOutputs(request) => {
                (request.end_version, &advertised_data.transaction_outputs)
            }
            request => invalid_stream_request!(request),
        };
        AdvertisedData::contains_range(
            self.next_stream_version,
            request_end_version,
            advertised_ranges,
        )
    }

    fn transform_client_response_into_notification(
        &mut self,
        client_request: &DataClientRequest,
        client_response: &DataClientResponse,
        notification_id_generator: Arc<U64IdGenerator>,
    ) -> Result<Option<DataNotification>, Error> {
        match &self.request {
            StreamRequest::GetAllTransactions(_) => match client_request {
                TransactionsWithProof(request) => {
                    self.update_stream_version(request.start_version, request.end_version)?;
                }
                request => invalid_client_request!(request, self),
            },
            StreamRequest::GetAllTransactionOutputs(_) => match client_request {
                TransactionOutputsWithProof(request) => {
                    self.update_stream_version(request.start_version, request.end_version)?;
                }
                request => invalid_client_request!(request, self),
            },
            request => invalid_stream_request!(request),
        }

        // Create a new data notification
        let data_notification = create_data_notification(
            notification_id_generator,
            client_response,
            self.clone().into(),
        );
        Ok(Some(data_notification))
    }
}

/// Verifies that the `expected_next_index` matches the `start_index` and that
/// the `end_index` is greater than or equal to `expected_next_index`.
fn verify_client_request_indices(expected_next_index: u64, start_index: u64, end_index: u64) {
    if start_index != expected_next_index {
        panic!(
            "The start index did not match the expected next index! Given: {:?}, expected: {:?}",
            start_index, expected_next_index
        );
    }
    if end_index < expected_next_index {
        panic!(
            "The end index was less than the expected next index! Given: {:?}, expected: {:?}",
            end_index, expected_next_index
        );
    }
}

/// Creates a batch of data client requests for the given stream progress tracker
fn create_data_client_requests(
    start_index: u64,
    end_index: u64,
    max_number_of_requests: u64,
    optimal_chunk_size: u64,
    stream_progress_tracker: StreamProgressTracker,
) -> Result<Vec<DataClientRequest>, Error> {
    if start_index > end_index {
        // TODO(joshlind): log this occurrence! We need to handle stream completion.
        return Ok(vec![]);
    }

    // Calculate the total number of items left to satisfy the stream
    let mut total_items_to_fetch = end_index
        .checked_sub(start_index)
        .and_then(|e| e.checked_add(1)) // = end_index - start_index + 1
        .ok_or_else(|| Error::IntegerOverflow("Total items to fetch has overflown!".into()))?;

    // Iterate until we've requested all transactions or hit the maximum number of requests
    let mut data_client_requests = vec![];
    let mut num_requests_made = 0;
    let mut next_index_to_request = start_index;
    while total_items_to_fetch > 0 && num_requests_made < max_number_of_requests {
        // Calculate the number of items to fetch in this request
        let num_items_to_fetch = cmp::min(total_items_to_fetch, optimal_chunk_size);

        // Calculate the start and end indices for the request
        let request_start_index = next_index_to_request;
        let request_end_index = request_start_index
            .checked_add(num_items_to_fetch)
            .and_then(|e| e.checked_sub(1)) // = request_start_index + num_items_to_fetch - 1
            .ok_or_else(|| Error::IntegerOverflow("End index to fetch has overflown!".into()))?;

        // Create the data client requests
        let data_client_request = create_data_client_request(
            request_start_index,
            request_end_index,
            &stream_progress_tracker,
        );
        data_client_requests.push(data_client_request);

        // Update the local loop state
        next_index_to_request = request_end_index
            .checked_add(1)
            .ok_or_else(|| Error::IntegerOverflow("Next index to request has overflown!".into()))?;
        total_items_to_fetch = total_items_to_fetch
            .checked_sub(num_items_to_fetch)
            .ok_or_else(|| Error::IntegerOverflow("Total items to fetch has overflown!".into()))?;
        num_requests_made = num_requests_made.checked_add(1).ok_or_else(|| {
            Error::IntegerOverflow("Number of payload requests has overflown!".into())
        })?;
    }

    Ok(data_client_requests)
}

/// Creates a data client request for the given stream tracker using the
/// specified start and end indices.
fn create_data_client_request(
    start_index: u64,
    end_index: u64,
    stream_progress_tracker: &StreamProgressTracker,
) -> DataClientRequest {
    match stream_progress_tracker {
        StreamProgressTracker::AccountsStreamTracker(stream_tracker) => {
            DataClientRequest::AccountsWithProof(AccountsWithProofRequest {
                version: stream_tracker.request.version,
                start_index,
                end_index,
            })
        }
        StreamProgressTracker::ContinuousTransactionStreamTracker(stream_tracker) => {
            let target_ledger_info_version = stream_tracker
                .get_target_ledger_info()
                .ledger_info()
                .version();
            match &stream_tracker.request {
                StreamRequest::ContinuouslyStreamTransactions(request) => {
                    DataClientRequest::TransactionsWithProof(TransactionsWithProofRequest {
                        start_version: start_index,
                        end_version: end_index,
                        max_proof_version: target_ledger_info_version,
                        include_events: request.include_events,
                    })
                }
                StreamRequest::ContinuouslyStreamTransactionOutputs(_) => {
                    DataClientRequest::TransactionOutputsWithProof(
                        TransactionOutputsWithProofRequest {
                            start_version: start_index,
                            end_version: end_index,
                            max_proof_version: target_ledger_info_version,
                        },
                    )
                }
                request => invalid_stream_request!(request),
            }
        }
        StreamProgressTracker::EpochEndingStreamTracker(_) => {
            DataClientRequest::EpochEndingLedgerInfos(EpochEndingLedgerInfosRequest {
                start_epoch: start_index,
                end_epoch: end_index,
            })
        }
        StreamProgressTracker::TransactionStreamTracker(stream_tracker) => {
            match &stream_tracker.request {
                StreamRequest::GetAllTransactions(request) => {
                    DataClientRequest::TransactionsWithProof(TransactionsWithProofRequest {
                        start_version: start_index,
                        end_version: end_index,
                        max_proof_version: request.max_proof_version,
                        include_events: request.include_events,
                    })
                }
                StreamRequest::GetAllTransactionOutputs(request) => {
                    DataClientRequest::TransactionOutputsWithProof(
                        TransactionOutputsWithProofRequest {
                            start_version: start_index,
                            end_version: end_index,
                            max_proof_version: request.max_proof_version,
                        },
                    )
                }
                request => invalid_stream_request!(request),
            }
        }
    }
}

/// Returns the most common highest epoch advertised in the network.
/// Note: we use this to reduce the likelihood of malicious nodes
/// interfering with syncing progress by advertising non-existent epochs.
fn most_common_highest_epoch(advertised_data: &AdvertisedData) -> Option<Epoch> {
    // Count the frequencies of the highest epochs
    let highest_epoch_frequencies = advertised_data
        .epoch_ending_ledger_infos
        .iter()
        .map(|epoch_range| epoch_range.highest())
        .clone()
        .counts();

    // Return the most common epoch
    highest_epoch_frequencies
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(epoch, _)| epoch)
}

/// Returns the highest synced ledger info advertised in the network
fn highest_synced_ledger_info(
    advertised_data: &AdvertisedData,
) -> Option<LedgerInfoWithSignatures> {
    let highest_synced_position = advertised_data
        .synced_ledger_infos
        .iter()
        .map(|ledger_info_with_sigs| ledger_info_with_sigs.ledger_info().version())
        .position_max();

    if let Some(highest_synced_position) = highest_synced_position {
        advertised_data
            .synced_ledger_infos
            .get(highest_synced_position)
            .cloned()
    } else {
        None
    }
}

/// Creates a new data notification for the given client response.
fn create_data_notification(
    notification_id_generator: Arc<U64IdGenerator>,
    client_response: &DataClientResponse,
    stream_progress_tracker: StreamProgressTracker,
) -> DataNotification {
    let notification_id = notification_id_generator.next();

    let data_payload = match &client_response.payload {
        ResponsePayload::AccountStatesWithProof(accounts_chunk) => {
            DataPayload::AccountStatesWithProof(accounts_chunk.clone())
        }
        ResponsePayload::EpochEndingLedgerInfos(ledger_infos) => {
            DataPayload::EpochEndingLedgerInfos(ledger_infos.clone())
        }
        ResponsePayload::TransactionsWithProof(transactions_chunk) => match stream_progress_tracker
        {
            StreamProgressTracker::ContinuousTransactionStreamTracker(stream_tracker) => {
                DataPayload::ContinuousTransactionsWithProof(
                    stream_tracker.get_target_ledger_info().clone(),
                    transactions_chunk.clone(),
                )
            }
            StreamProgressTracker::TransactionStreamTracker(_) => {
                DataPayload::TransactionsWithProof(transactions_chunk.clone())
            }
            _ => invalid_response_type!(client_response),
        },
        ResponsePayload::TransactionOutputsWithProof(transactions_output_chunk) => {
            match stream_progress_tracker {
                StreamProgressTracker::ContinuousTransactionStreamTracker(stream_tracker) => {
                    DataPayload::ContinuousTransactionOutputsWithProof(
                        stream_tracker.get_target_ledger_info().clone(),
                        transactions_output_chunk.clone(),
                    )
                }
                StreamProgressTracker::TransactionStreamTracker(_) => {
                    DataPayload::TransactionOutputsWithProof(transactions_output_chunk.clone())
                }
                _ => invalid_response_type!(client_response),
            }
        }
        _ => invalid_response_type!(client_response),
    };

    DataNotification {
        notification_id,
        data_payload,
    }
}
