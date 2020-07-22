use crate::{
    compute_enc_pending_balance, confidential_transaction_file, construct_path,
    create_rng_from_seed, debug_decrypt, errors::Error, last_ordering_state_before, load_object,
    save_object, user_public_account_file, user_secret_account_file, CTXInstruction, Instruction,
    COMMON_OBJECTS_DIR, MEDIATOR_PUBLIC_ACCOUNT_FILE, OFF_CHAIN_DIR, ON_CHAIN_DIR,
};
use codec::{Decode, Encode};
use cryptography::mercat::{
    transaction::{CtxReceiver, CtxSender},
    Account, AccountMemo, InitializedTx, PubAccount, TransactionReceiver, TransactionSender,
    TxState, TxSubstate,
};
use lazy_static::lazy_static;
use log::{debug, info};
use metrics::timing;
use rand::Rng;
use schnorrkel::{context::SigningContext, signing_context};
use std::{path::PathBuf, time::Instant};

lazy_static! {
    static ref SIG_CTXT: SigningContext = signing_context(b"mercat/transaction");
}

pub fn process_create_tx(
    seed: String,
    db_dir: PathBuf,
    sender: String,
    receiver: String,
    mediator: String,
    ticker: String,
    amount: u32,
    tx_id: u32,
    cheat: bool,
) -> Result<(), Error> {
    let mut rng = create_rng_from_seed(Some(seed))?;
    let load_from_file_timer = Instant::now();

    let sender_account = Account {
        scrt: load_object(
            db_dir.clone(),
            OFF_CHAIN_DIR,
            &sender,
            &user_secret_account_file(&ticker),
        )?,
        pblc: load_object(
            db_dir.clone(),
            ON_CHAIN_DIR,
            &sender,
            &user_public_account_file(&ticker),
        )?,
    };

    let receiver_account: PubAccount = load_object(
        db_dir.clone(),
        ON_CHAIN_DIR,
        &receiver,
        &user_public_account_file(&ticker),
    )?;

    let mediator_account: AccountMemo = load_object(
        db_dir.clone(),
        ON_CHAIN_DIR,
        &mediator,
        MEDIATOR_PUBLIC_ACCOUNT_FILE,
    )?;

    timing!(
        "account.create_tx.load_from_file",
        load_from_file_timer,
        Instant::now()
    );

    // Calculate the pending
    let calc_pending_state_timer = Instant::now();
    let last_processed_tx_counter = sender_account.pblc.memo.last_processed_tx_counter;
    let last_processed_account_balance = sender_account.pblc.enc_balance;
    let ordering_state = last_ordering_state_before(
        sender.clone(),
        last_processed_tx_counter,
        tx_id,
        tx_id,
        db_dir.clone(),
    )?;

    let pending_balance = compute_enc_pending_balance(
        &sender,
        ordering_state,
        last_processed_tx_counter,
        last_processed_account_balance,
        db_dir.clone(),
    )?;
    debug!(
        "------------> initiating transfer tx: {}, pending_balance: {}",
        tx_id,
        debug_decrypt(
            sender_account.pblc.id,
            pending_balance.clone(),
            db_dir.clone()
        )?
    );
    let next_pending_tx_counter = ordering_state.last_pending_tx_counter + 1;

    timing!(
        "account.create_tx.calc_pending_state",
        calc_pending_state_timer,
        Instant::now()
    );

    let mut amount = amount;
    // To simplify the cheating selection process, we randomly choose a cheating strategy,
    // instead of requiring the caller to know of all the different cheating strategies.
    let cheating_strategy: u32 = rng.gen_range(0, 2);

    // The first cheating strategies make changes to the input, while the subsequent ones
    // changes the output.
    if cheat && cheating_strategy == 0 {
        info!(
            "CLI log: tx-{}: Cheating by changing the agreed upon amount. Correct amount: {}",
            tx_id, amount
        );
        amount += 1
    }

    // Initialize the transaction.
    let create_tx_timer = Instant::now();
    let ctx_sender = CtxSender {};
    let mut asset_tx = ctx_sender
        .create_transaction(
            tx_id,
            &sender_account,
            &receiver_account,
            &mediator_account.owner_enc_pub_key,
            pending_balance,
            amount,
            next_pending_tx_counter,
            &mut rng,
        )
        .map_err(|error| Error::LibraryError { error })?;
    timing!("account.create_tx.create", create_tx_timer, Instant::now());

    if cheat && cheating_strategy == 1 {
        info!(
            "CLI log: tx-{}: Cheating by changing the sender's account id. Correct account id: {}",
            tx_id, sender_account.pblc.id
        );
        asset_tx.content.memo.sndr_account_id += 1;
        let message = asset_tx.content.encode();
        asset_tx.sig = sender_account.scrt.sign_keys.sign(SIG_CTXT.bytes(&message));
    }

    // Save the artifacts to file.
    let new_state = TxState::Initialization(TxSubstate::Started);
    let save_to_file_timer = Instant::now();
    let instruction = CTXInstruction {
        state: new_state,
        data: asset_tx.encode().to_vec(),
    };

    // TODO(CRYP-127)
    // We should name the transactions based on the ordering counters, or we may decide that
    // a global counter (tx_id) is enough and put all transactions inside a common folder.
    save_object(
        db_dir,
        ON_CHAIN_DIR,
        COMMON_OBJECTS_DIR,
        &confidential_transaction_file(tx_id, &sender, new_state),
        &instruction,
    )?;

    timing!(
        "account.create_tx.save_to_file",
        save_to_file_timer,
        Instant::now()
    );

    Ok(())
}

pub fn process_finalize_tx(
    seed: String,
    db_dir: PathBuf,
    sender: String,
    receiver: String,
    ticker: String,
    amount: u32,
    tx_id: u32,
    cheat: bool,
) -> Result<(), Error> {
    let mut rng = create_rng_from_seed(Some(seed))?;
    let load_from_file_timer = Instant::now();
    let state = TxState::Initialization(TxSubstate::Started);

    let sender_account: PubAccount = load_object(
        db_dir.clone(),
        ON_CHAIN_DIR,
        &sender,
        &user_public_account_file(&ticker),
    )?;

    let receiver_account = Account {
        scrt: load_object(
            db_dir.clone(),
            OFF_CHAIN_DIR,
            &receiver,
            &user_secret_account_file(&ticker),
        )?,
        pblc: load_object(
            db_dir.clone(),
            ON_CHAIN_DIR,
            &receiver,
            &user_public_account_file(&ticker),
        )?,
    };

    let instruction: Instruction = load_object(
        db_dir.clone(),
        ON_CHAIN_DIR,
        COMMON_OBJECTS_DIR,
        &confidential_transaction_file(tx_id.clone(), &sender, state),
    )?;

    let tx = InitializedTx::decode(&mut &instruction.data[..]).map_err(|error| {
        Error::ObjectLoadError {
            error,
            path: construct_path(
                db_dir.clone(),
                ON_CHAIN_DIR,
                &sender.clone(),
                &confidential_transaction_file(tx_id.clone(), &sender, state),
            ),
        }
    })?;

    timing!(
        "account.finalize_tx.load_from_file",
        load_from_file_timer,
        Instant::now()
    );

    // Calculate the pending
    let calc_pending_state_timer = Instant::now();
    let ordering_state = last_ordering_state_before(
        receiver,
        receiver_account.pblc.memo.last_processed_tx_counter,
        tx_id,
        tx_id,
        db_dir.clone(),
    )?;
    let next_pending_tx_counter = ordering_state.last_pending_tx_counter + 1;

    timing!(
        "account.finalize_tx.calc_pending_state",
        calc_pending_state_timer,
        Instant::now()
    );

    let mut amount = amount;
    // To simplify the cheating selection process, we randomly choose a cheating strategy,
    // instead of requiring the caller to know of all the different cheating strategies.
    let cheating_strategy: u32 = rng.gen_range(0, 2);

    // The first cheating strategies make changes to the input, while the 2nd one
    // changes the output.
    if cheat && cheating_strategy == 0 {
        info!(
            "CLI log: tx-{}: Cheating by changing the agreed upon amount. Correct amount: {}",
            tx_id, amount
        );
        amount += 1
    }

    // Finalize the transaction.
    let finalize_by_receiver_timer = Instant::now();
    let receiver = CtxReceiver {};
    let mut asset_tx = receiver
        .finalize_transaction(
            tx_id,
            tx,
            &sender_account.memo.owner_sign_pub_key,
            receiver_account.clone(),
            amount,
            next_pending_tx_counter,
            &mut rng,
        )
        .map_err(|error| Error::LibraryError { error })?;

    if cheat && cheating_strategy == 1 {
        info!(
            "CLI log: tx-{}: Cheating by changing the sender's account id. Correct account id: {}",
            tx_id, receiver_account.pblc.id
        );
        asset_tx.content.init_data.content.memo.rcvr_account_id += 1;
        let message = asset_tx.content.encode();
        asset_tx.sig = receiver_account
            .scrt
            .sign_keys
            .sign(SIG_CTXT.bytes(&message));
    }

    timing!(
        "account.finalize_tx.finalize_by_receiver",
        finalize_by_receiver_timer,
        Instant::now()
    );

    // Save the artifacts to file.
    let save_to_file_timer = Instant::now();
    let state = TxState::Finalization(TxSubstate::Started);
    let instruction = CTXInstruction {
        state,
        data: asset_tx.encode().to_vec(),
    };

    // TODO(CRYP-127)
    // We should name the transactions based on the ordering counters, or we may decide that
    // a global counter (tx_id) is enough and put all transactions inside a common folder.
    save_object(
        db_dir,
        ON_CHAIN_DIR,
        COMMON_OBJECTS_DIR,
        &confidential_transaction_file(tx_id, &sender, state),
        &instruction,
    )?;

    timing!(
        "account.finalize_tx.save_to_file",
        save_to_file_timer,
        Instant::now()
    );

    Ok(())
}
