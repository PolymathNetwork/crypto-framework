use crate::{
    account_create_transaction_file, all_unverified_tx_files, asset_transaction_file,
    compute_enc_pending_balance, confidential_transaction_file, debug_decrypt, errors::Error,
    get_asset_ids, get_user_ticker_from, last_ordering_state, load_object, load_tx_file,
    parse_tx_name, save_object, save_to_file, user_public_account_file, AssetInstruction,
    CoreTransaction, Direction, OrderedPubAccount, OrderedPubAccountTx, TransferInstruction,
    ValidationResult, COMMON_OBJECTS_DIR, LAST_VALIDATED_TX_ID_FILE, MEDIATOR_PUBLIC_ACCOUNT_FILE,
    OFF_CHAIN_DIR, ON_CHAIN_DIR,
};
use codec::{Decode, Encode};
use cryptography::mercat::{
    account::AccountValidator, asset::AssetValidator, transaction::TransactionValidator,
    AccountCreatorVerifier, AccountMemo, AssetTransactionVerifier, AssetTxState, EncryptedAmount,
    JustifiedAssetTx, JustifiedTransferTx, PubAccount, TransferTransactionVerifier,
    TransferTxState, TxSubstate,
};
use log::{debug, error, info};
use metrics::timing;
use rand::rngs::OsRng;
use std::{collections::HashSet, path::PathBuf, time::Instant};

fn load_all_unverified_and_ready(db_dir: PathBuf) -> Result<Vec<CoreTransaction>, Error> {
    all_unverified_tx_files(db_dir)?
        .into_iter()
        .map(|tx| parse_tx_name(tx))
        .map(|res| match res {
            Err(error) => Err(error),
            Ok((tx_id, user, state, tx_file_path)) => {
                load_tx_file(tx_id, user, state, tx_file_path)
            }
        })
        .filter(|res| res.is_err() || res.as_ref().unwrap().is_ready_for_validation())
        .collect()
}

pub fn validate_all_pending(db_dir: PathBuf) -> Result<(), Error> {
    // TODO: This function should be called when any justify is called. To be fixed in CRYP-131.
    let all_unverified_and_ready = load_all_unverified_and_ready(db_dir.clone())?;
    let mut last_tx_id: Option<u32> = None;

    let mut results: Vec<ValidationResult> = vec![];
    // For each of them call the validate function and process as needed
    for tx in all_unverified_and_ready {
        match tx {
            CoreTransaction::IssueJustify {
                issue_tx,
                tx_id,
                mediator,
            } => {
                let result =
                    validate_asset_issuance(db_dir.clone(), issue_tx.clone(), mediator, tx_id);
                results.push(result);
                last_tx_id = Some(std::cmp::max(last_tx_id.unwrap_or_default(), tx_id));
            }
            CoreTransaction::TransferJustify {
                tx,
                tx_id,
                mediator,
            } => {
                let account_id = tx.content.content.init_data.content.memo.sndr_account_id;
                let (sender, ticker, _) = get_user_ticker_from(account_id, db_dir.clone())?;
                let sender_ordered_pub_account: OrderedPubAccount = load_object(
                    db_dir.clone(),
                    ON_CHAIN_DIR,
                    &sender,
                    &user_public_account_file(&ticker),
                )?;
                let ordering_state = last_ordering_state(
                    sender.clone(),
                    sender_ordered_pub_account.last_processed_tx_counter,
                    tx_id,
                    db_dir.clone(),
                )?;
                let pending_balance = compute_enc_pending_balance(
                    &sender,
                    ordering_state,
                    sender_ordered_pub_account.last_processed_tx_counter,
                    sender_ordered_pub_account.pub_account.enc_balance,
                    db_dir.clone(),
                )?;
                debug!(
                    "------------> validating tx: {}, pending transfer balance: {}",
                    tx_id,
                    debug_decrypt(account_id, pending_balance.clone(), db_dir.clone())?
                );
                let (sender_result, receiver_result) =
                    validate_transaction(db_dir.clone(), tx, mediator, pending_balance, tx_id);
                results.push(sender_result);
                results.push(receiver_result);
                last_tx_id = Some(std::cmp::max(last_tx_id.unwrap_or_default(), tx_id));
            }
            CoreTransaction::Account {
                account_tx,
                tx_id,
                ordering_state: _,
            } => {
                match validate_account(db_dir.clone(), account_tx.content.pub_account.id) {
                    Err(error) => {
                        error!("Error in validation of tx-{}: {:#?}", tx_id, error);
                        error!("tx-{}: Ignoring the validation error and continuing the with rest of the validations.", tx_id);
                    }
                    Ok(_) => (),
                };
                last_tx_id = Some(std::cmp::max(last_tx_id.unwrap_or_default(), tx_id));
            }
            _ => {
                return Err(Error::TransactionIsNotReadyForValidation { tx });
            }
        }
    }

    // TODO: CRYP-134, use a more elegant way of writing the following code.

    // find all users
    let mut users: Vec<String> = vec![];
    for result in results.clone() {
        if result.user != "n/a" {
            users.push(result.user);
        }
    }
    // find all accounts
    let mut accounts: HashSet<(String, String)> = HashSet::new();
    for user in users {
        for result in results.clone() {
            if result.user == user {
                accounts.insert((result.user, result.ticker));
            }
        }
    }

    for (user, ticker) in accounts.clone() {
        let ordered_pub_account: OrderedPubAccount = load_object(
            db_dir.clone(),
            ON_CHAIN_DIR,
            &user,
            &user_public_account_file(&ticker),
        )?;
        let mut new_balance = ordered_pub_account.pub_account.enc_balance;
        debug!(
            "------------> Validation complete, updating {}-{}. Starting balance: {}",
            &user,
            &ticker,
            debug_decrypt(
                ordered_pub_account.pub_account.id,
                new_balance.clone(),
                db_dir.clone()
            )?
        );
        for result in results.clone() {
            if result.user == user && result.ticker == ticker {
                match result.direction {
                    Direction::Incoming => {
                        if let Some(amount) = result.amount {
                            debug!(
                                "---------------------> updating {}-{} increasing by {}",
                                &user,
                                &ticker,
                                debug_decrypt(
                                    ordered_pub_account.pub_account.id,
                                    amount.clone(),
                                    db_dir.clone()
                                )?
                            );
                            new_balance += amount.clone();
                        } else {
                            // based on the reason and the strategy, we can break the loop or ignore
                            // TODO: add strategy selection to the config. CRYP-132
                        }
                    }
                    Direction::Outgoing => {
                        if let Some(amount) = result.amount {
                            debug!(
                                "---------------------> updating {}-{} decreasing by {}",
                                &user,
                                &ticker,
                                debug_decrypt(
                                    ordered_pub_account.pub_account.id,
                                    amount.clone(),
                                    db_dir.clone()
                                )?
                            );
                            new_balance -= amount.clone();
                        } else {
                            // based on the reason and the strategy, we can break the loop or ignore
                        }
                    }
                }
            }
        }

        save_object(
            db_dir.clone(),
            ON_CHAIN_DIR,
            &user,
            &user_public_account_file(&ticker),
            &OrderedPubAccount {
                last_processed_tx_counter: last_tx_id,
                pub_account: PubAccount {
                    id: ordered_pub_account.pub_account.id,
                    enc_asset_id: ordered_pub_account.pub_account.enc_asset_id,
                    enc_balance: new_balance,
                    memo: ordered_pub_account.pub_account.memo,
                },
            },
        )?;
    }

    save_to_file(
        db_dir,
        OFF_CHAIN_DIR,
        COMMON_OBJECTS_DIR,
        LAST_VALIDATED_TX_ID_FILE,
        &last_tx_id,
    )?;
    Ok(())
}

pub fn validate_asset_issuance(
    db_dir: PathBuf,
    asset_tx: JustifiedAssetTx,
    mediator: String,
    tx_id: u32,
) -> ValidationResult {
    let load_objects_timer = Instant::now();

    let issuer_account_id = asset_tx.content.content.account_id;
    let res = get_user_ticker_from(issuer_account_id, db_dir.clone());
    if let Err(error) = res {
        error!("Error in validation of tx-{}: {:#?}", tx_id, error);
        return ValidationResult::error("n/a", "n/a");
    }
    let (issuer, ticker, _) = res.unwrap();
    info!(
        "Validating asset issuance{{tx_id: {}, issuer: {}, ticker: {}, mediator: {}}}",
        tx_id, issuer, ticker, mediator
    );
    let mediator_account: Result<AccountMemo, Error> = load_object(
        db_dir.clone(),
        ON_CHAIN_DIR,
        &mediator,
        MEDIATOR_PUBLIC_ACCOUNT_FILE,
    );
    if let Err(error) = mediator_account {
        error!("Error in validation of tx-{}: {:#?}", tx_id, error);
        return ValidationResult::error(&issuer, &ticker);
    }
    let mediator_account = mediator_account.unwrap();

    let issuer_ordered_pub_account: Result<OrderedPubAccount, Error> = load_object(
        db_dir.clone(),
        ON_CHAIN_DIR,
        &issuer,
        &user_public_account_file(&ticker),
    );
    if let Err(error) = issuer_ordered_pub_account {
        error!("Error in validation of tx-{}: {:#?}", tx_id, error);
        return ValidationResult::error(&issuer, &ticker);
    }
    let issuer_ordered_pub_account = issuer_ordered_pub_account.unwrap();

    timing!(
        "validator.issuance.load_objects",
        load_objects_timer,
        Instant::now(),
        "tx_id" => tx_id.to_string()
    );

    let validate_issuance_transaction_timer = Instant::now();

    let validator = AssetValidator {};
    let _ = match validator
        .verify_asset_transaction(
            &asset_tx,
            issuer_ordered_pub_account.pub_account,
            &mediator_account.owner_enc_pub_key,
            &mediator_account.owner_sign_pub_key,
            &[],
        )
        .map_err(|error| Error::LibraryError { error })
    {
        Err(error) => {
            error!("Error in validation of tx-{}: {:#?}", tx_id, error);
            return ValidationResult::error(&issuer, &ticker);
        }
        Ok(pub_account) => pub_account,
    };

    timing!(
        "validator.issuance.transaction",
        validate_issuance_transaction_timer,
        Instant::now(),
        "tx_id" => tx_id.to_string()
    );

    let save_objects_timer = Instant::now();
    // Save the transaction under the new state.
    let new_state = AssetTxState::Justification(TxSubstate::Validated);
    let instruction = AssetInstruction {
        state: new_state,
        data: asset_tx.encode().to_vec(),
    };
    if let Err(error) = save_object(
        db_dir.clone(),
        ON_CHAIN_DIR,
        &issuer,
        &asset_transaction_file(tx_id, &issuer, new_state),
        &instruction,
    ) {
        error!("Error in validation of tx-{}: {:#?}", tx_id, error);
        return ValidationResult::error(&issuer, &ticker);
    }

    timing!(
        "validator.issuance.save_objects",
        save_objects_timer,
        Instant::now(),
        "tx_id" => tx_id.to_string()
    );

    ValidationResult {
        user: issuer,
        ticker,
        amount: Some(asset_tx.content.content.memo.enc_issued_amount),
        direction: Direction::Incoming,
    }
}

pub fn validate_account(db_dir: PathBuf, account_id: u32) -> Result<(), Error> {
    // Load the user's public account.
    let load_objects_timer = Instant::now();

    let (user, ticker, tx_id) = get_user_ticker_from(account_id, db_dir.clone())?;
    info!(
        "Validating account{{tx_id: {}, account_id: {}, user: {}, ticker: {}}}",
        tx_id, account_id, user, ticker
    );
    let ordered_user_account_tx: OrderedPubAccountTx = load_object(
        db_dir.clone(),
        ON_CHAIN_DIR,
        COMMON_OBJECTS_DIR,
        &account_create_transaction_file(tx_id, &user, &ticker),
    )?;

    let valid_asset_ids = get_asset_ids(db_dir.clone())?;
    timing!(
        "validator.account.load_objects",
        load_objects_timer,
        Instant::now(),
        "tx_id" => tx_id.to_string()
    );

    // Validate the account.
    let validate_account_timer = Instant::now();
    let account_validator = AccountValidator {};
    account_validator
        .verify(&ordered_user_account_tx.account_tx, &valid_asset_ids)
        .map_err(|error| Error::LibraryError { error })?;

    timing!(
        "validator.account",
        validate_account_timer,
        Instant::now(),
        "tx_id" => tx_id.to_string()
    );

    // On success save the public account as validated.
    let save_objects_timer = Instant::now();
    let ordered_account = OrderedPubAccount {
        pub_account: ordered_user_account_tx.account_tx.content.pub_account,
        last_processed_tx_counter: Some(tx_id),
    };
    save_object(
        db_dir,
        ON_CHAIN_DIR,
        &user,
        &user_public_account_file(&ticker),
        &ordered_account,
    )?;

    timing!(
        "validator.account.save_objects",
        save_objects_timer,
        Instant::now(),
        "tx_id" => tx_id.to_string()
    );

    Ok(())
}

fn process_transaction(
    instruction: TransferInstruction,
    sender_pub_account: PubAccount,
    receiver_pub_account: PubAccount,
    mdtr_account: &AccountMemo,
    pending_balance: EncryptedAmount,
) -> Result<(PubAccount, PubAccount), Error> {
    let mut rng = OsRng::default();
    let tx = JustifiedTransferTx::decode(&mut &instruction.data[..]).unwrap();
    let validator = TransactionValidator {};
    let (updated_sender_account, updated_receiver_account) = validator
        .verify_transaction(
            &tx,
            sender_pub_account,
            receiver_pub_account,
            &mdtr_account.owner_sign_pub_key,
            pending_balance,
            &[],
            &mut rng,
        )
        .map_err(|error| Error::LibraryError { error })?;

    Ok((updated_sender_account, updated_receiver_account))
}

pub fn validate_transaction(
    db_dir: PathBuf,
    tx: JustifiedTransferTx,
    mediator: String,
    pending_balance: EncryptedAmount,
    tx_id: u32,
) -> (ValidationResult, ValidationResult) {
    let load_objects_timer = Instant::now();
    // Load the transaction, mediator's account, and issuer's public account.

    let (sender, _, _) = match get_user_ticker_from(
        tx.content.content.init_data.content.memo.sndr_account_id,
        db_dir.clone(),
    ) {
        Err(error) => {
            error!("Error in validation of tx-{}: {:#?}", tx_id, error);
            return (
                ValidationResult::error("n/a", "n/a"),
                ValidationResult::error("n/a", "n/a"),
            );
        }
        Ok(ok) => ok,
    };

    let (receiver, ticker, _) = match get_user_ticker_from(
        tx.content.content.init_data.content.memo.rcvr_account_id,
        db_dir.clone(),
    ) {
        Err(error) => {
            error!("Error in validation of tx-{}: {:#?}", tx_id, error);
            return (
                ValidationResult::error("n/a", "n/a"),
                ValidationResult::error("n/a", "n/a"),
            );
        }
        Ok(ok) => ok,
    };

    info!(
        "Validating asset transfer{{tx_id: {}, sender: {}, receiver: {}, ticker:{}, mediator: {}}}",
        tx_id, sender, receiver, ticker, mediator
    );
    let state = TransferTxState::Justification(TxSubstate::Started);

    let mut instruction: TransferInstruction = match load_object(
        db_dir.clone(),
        ON_CHAIN_DIR,
        COMMON_OBJECTS_DIR,
        &confidential_transaction_file(tx_id, &mediator, state),
    ) {
        Err(error) => {
            error!("Error in validation of tx-{}: {:#?}", tx_id, error);
            return (
                ValidationResult::error(&sender, &ticker),
                ValidationResult::error(&receiver, &ticker),
            );
        }
        Ok(ok) => ok,
    };

    let mediator_account: AccountMemo = match load_object(
        db_dir.clone(),
        ON_CHAIN_DIR,
        &mediator,
        MEDIATOR_PUBLIC_ACCOUNT_FILE,
    ) {
        Err(error) => {
            error!("Error in validation of tx-{}: {:#?}", tx_id, error);
            return (
                ValidationResult::error(&sender, &ticker),
                ValidationResult::error(&receiver, &ticker),
            );
        }
        Ok(ok) => ok,
    };

    let sender_ordered_pub_account: OrderedPubAccount = match load_object(
        db_dir.clone(),
        ON_CHAIN_DIR,
        &sender,
        &user_public_account_file(&ticker),
    ) {
        Err(error) => {
            error!("Error in validation of tx-{}: {:#?}", tx_id, error);
            return (
                ValidationResult::error(&sender, &ticker),
                ValidationResult::error(&receiver, &ticker),
            );
        }
        Ok(ok) => ok,
    };

    let receiver_ordered_pub_account: OrderedPubAccount = match load_object(
        db_dir.clone(),
        ON_CHAIN_DIR,
        &receiver,
        &user_public_account_file(&ticker),
    ) {
        Err(error) => {
            error!("Error in validation of tx-{}: {:#?}", tx_id, error);
            return (
                ValidationResult::error(&sender, &ticker),
                ValidationResult::error(&receiver, &ticker),
            );
        }
        Ok(ok) => ok,
    };

    timing!(
        "validator.issuance.load_objects",
        load_objects_timer,
        Instant::now(),
        "tx_id" => tx_id.to_string()
    );

    let validate_transaction_timer = Instant::now();
    let (_, _) = match process_transaction(
        instruction.clone(),
        sender_ordered_pub_account.pub_account,
        receiver_ordered_pub_account.pub_account,
        &mediator_account,
        pending_balance,
    ) {
        Err(error) => {
            error!("Error in validation of tx-{}: {:#?}", tx_id, error);
            return (
                ValidationResult::error(&sender, &ticker),
                ValidationResult::error(&receiver, &ticker),
            );
        }
        Ok(ok) => ok,
    };

    timing!(
        "validator.transaction",
        validate_transaction_timer,
        Instant::now(),
        "tx_id" =>  tx_id.to_string()
    );

    let save_objects_timer = Instant::now();
    // Save the transaction under the new state.
    instruction.state = TransferTxState::Justification(TxSubstate::Validated);
    if let Err(error) = save_object(
        db_dir.clone(),
        ON_CHAIN_DIR,
        COMMON_OBJECTS_DIR,
        &confidential_transaction_file(tx_id, &sender, instruction.state),
        &instruction,
    ) {
        error!("Error in validation of tx-{}: {:#?}", tx_id, error);
        return (
            ValidationResult::error(&sender, &ticker),
            ValidationResult::error(&receiver, &ticker),
        );
    }

    timing!(
        "validator.issuance.save_objects",
        save_objects_timer,
        Instant::now(),
        "tx_id" => tx_id.to_string()
    );

    (
        ValidationResult {
            user: sender,
            ticker: ticker.clone(),
            direction: Direction::Outgoing,
            amount: Some(
                tx.content
                    .content
                    .init_data
                    .content
                    .memo
                    .enc_amount_using_sndr,
            ),
        },
        ValidationResult {
            user: receiver,
            ticker: ticker.clone(),
            direction: Direction::Incoming,
            amount: Some(
                tx.content
                    .content
                    .init_data
                    .content
                    .memo
                    .enc_amount_using_rcvr,
            ),
        },
    )
}
